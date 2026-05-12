use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, Identity, ReducerContext, Table};
use std::collections::BTreeSet;

use crate::cards::{self, cards as _cards_table};
use crate::packed::{pack_valid_at, valid_at_time};

/// Maximum byte length of a `Player.name`. Enforced by `validate_player_name`
/// on the input name and again after normalization in `claim_or_login`.
pub const MAX_PLAYER_NAME_LEN: usize = 64;

/// First `player_id` `next_player_id` will hand out on a fresh deployment.
/// Ids `0..FIRST_PLAYER_ID` are reserved for system / pseudo-players —
/// e.g., a "world" player that owns trees, rocks, and other unowned-by-
/// any-human world cards. Real players coming through `claim_or_login`
/// start at `FIRST_PLAYER_ID` and go up.
pub const FIRST_PLAYER_ID: u32 = 1024;

#[spacetimedb::table(accessor = players, public)]
#[derive(Debug, Clone)]
pub struct Player {
    /// Packed primary key — `[player_id: u32 | time_secs: u32]` (high | low).
    /// Multiple rows per `player_id` form a version history; the latest is
    /// the one with the largest `valid_at_time`.
    #[primary_key]
    pub valid_at: u64,
    #[index(btree)]
    pub player_id: u32,
    /// Display name. Match is case-sensitive — "Alice" and "alice" are
    /// different players. The history-style schema can't enforce uniqueness
    /// with `#[unique]` (multiple version rows per player would collide), so
    /// the registration reducer enforces it via lookup — see `claim_or_login`.
    #[index(btree)]
    pub name: String,
    /// Surface (formerly "layer") the player's soul currently occupies.
    /// `0` while the soul is not yet placed in the world.
    pub surface: u8,
    /// World macro_zone the soul currently occupies. `0` while unplaced.
    #[index(btree)]
    pub macro_zone: u32,
    /// In-zone position of the soul: `[local_q:u3][local_r:u3][stack_state:u2]`.
    /// `0` while unplaced.
    pub micro_zone: u8,
    /// Within-`micro_zone` position. Either a parent `card_id` (soul attached
    /// to another card) or packed `(i16 x, i16 y)` pixel coords (loose).
    /// `0` while unplaced.
    pub micro_location: u32,
}

/// Maps a connection's current `Identity` to the persistent `player_id`.
///
/// `Identity` is treated as ephemeral — a player who reconnects (or signs in
/// fresh) generally arrives with a new `Identity`. `claim_or_login` creates
/// or replaces the row; `client_disconnected` removes it. Regular reducers
/// go through `resolve_caller` to map `ctx.sender()` to the stable
/// `player_id`.
///
/// Private — clients have no need to subscribe.
#[spacetimedb::table(accessor = player_sessions)]
#[derive(Debug, Clone)]
pub struct PlayerSession {
    #[primary_key]
    pub identity: Identity,
    #[index(btree)]
    pub player_id: u32,
}

fn now_secs(ctx: &ReducerContext) -> u32 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000_000) as u32
}

// Latest row for a player_id is the row with the largest time component of valid_at.
pub fn latest(ctx: &ReducerContext, player_id: u32) -> Option<Player> {
    ctx.db
        .players()
        .player_id()
        .filter(player_id)
        .max_by_key(|p| valid_at_time(p.valid_at))
}

// Latest row whose `name` matches (case-sensitive). Same selection rule as
// `latest`, just keyed on a different btree index. Used by `claim_or_login`
// to resolve a name → player_id without scanning the whole table.
pub fn latest_by_name(ctx: &ReducerContext, name: &str) -> Option<Player> {
    ctx.db
        .players()
        .name()
        .filter(name)
        .max_by_key(|p| valid_at_time(p.valid_at))
}

// Stamp valid_at = (player_id, now) and write. Two writes within the same
// wall-clock second collide on the primary key; the existing row is replaced.
// Also enqueues a one-shot delete schedule that prunes older versions.
fn write(ctx: &ReducerContext, mut player: Player) -> Player {
    player.valid_at = pack_valid_at(player.player_id, now_secs(ctx));
    if ctx.db.players().valid_at().find(player.valid_at).is_some() {
        ctx.db.players().valid_at().delete(player.valid_at);
    }
    let inserted = ctx.db.players().insert(player);
    crate::schedule_delete_players::enqueue(ctx, inserted.player_id, inserted.valid_at);
    inserted
}

// Insert a brand-new player. valid_at is computed; pass 0 will be overwritten.
#[allow(clippy::too_many_arguments)]
pub fn create(
    ctx: &ReducerContext,
    player_id: u32,
    name: String,
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,
    micro_location: u32,
) -> Player {
    write(
        ctx,
        Player {
            valid_at: 0,
            player_id,
            name,
            surface,
            macro_zone,
            micro_zone,
            micro_location,
        },
    )
}

// Pick up the latest row for `player_id`, mutate it via `f`, write it back.
// Returns None if no prior row exists.
pub fn update_with<F>(ctx: &ReducerContext, player_id: u32, f: F) -> Option<Player>
where
    F: FnOnce(&mut Player),
{
    let mut p = latest(ctx, player_id)?;
    f(&mut p);
    Some(write(ctx, p))
}

// ---- single-field setters ---------------------------------------------

pub fn set_surface(ctx: &ReducerContext, player_id: u32, surface: u8) -> Option<Player> {
    update_with(ctx, player_id, |p| p.surface = surface)
}

pub fn set_macro_zone(ctx: &ReducerContext, player_id: u32, macro_zone: u32) -> Option<Player> {
    update_with(ctx, player_id, |p| p.macro_zone = macro_zone)
}

pub fn set_micro_zone(ctx: &ReducerContext, player_id: u32, micro_zone: u8) -> Option<Player> {
    update_with(ctx, player_id, |p| p.micro_zone = micro_zone)
}

pub fn set_micro_location(
    ctx: &ReducerContext,
    player_id: u32,
    micro_location: u32,
) -> Option<Player> {
    update_with(ctx, player_id, |p| p.micro_location = micro_location)
}

// ---- name handling ----------------------------------------------------

/// Validate a name before inserting it into a `Player` row.
///
/// Length is checked in bytes (not chars) since storage is what we're bounding.
/// Whitespace-only and control-character names are rejected so that no player
/// can render as blank or smuggle in characters that break logging / display.
pub fn validate_player_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("player name cannot be empty".to_string());
    }
    if name.trim().is_empty() {
        return Err("player name cannot be only whitespace".to_string());
    }
    if name.chars().any(|c| c.is_control()) {
        return Err("player name cannot contain control characters".to_string());
    }
    if name.len() > MAX_PLAYER_NAME_LEN {
        return Err(format!(
            "player name length {} exceeds max {}",
            name.len(),
            MAX_PLAYER_NAME_LEN,
        ));
    }
    Ok(())
}

// ---- session resolution ----------------------------------------------

/// Resolve the calling identity to a `player_id`. Returns `Err` if this
/// connection has not yet authenticated.
pub fn resolve_caller(ctx: &ReducerContext) -> Result<u32, String> {
    ctx.db
        .player_sessions()
        .identity()
        .find(ctx.sender())
        .map(|s| s.player_id)
        .ok_or_else(|| "caller has no active session".to_string())
}

// ---- player lifecycle -------------------------------------------------

/// Single-row counter table holding the next player_id to allocate.
/// Private — internal allocator state. Mirrors `CardIdCounter`.
#[spacetimedb::table(accessor = player_id_counter)]
pub struct PlayerIdCounter {
    #[primary_key]
    pub id: u8,
    pub next: u32,
}

/// Allocate the next `player_id` in O(1). Backed by a single-row
/// counter table; lazy-seeded from the current `max(player_id) + 1`
/// on the first call after a fresh deployment, O(1) thereafter.
///
/// Previously a full scan over `players` history — fine when small,
/// slow once every login or mutation has accumulated version rows.
fn next_player_id(ctx: &ReducerContext) -> u32 {
    if let Some(counter) = ctx.db.player_id_counter().id().find(0) {
        let allocated = counter.next;
        ctx.db.player_id_counter().id().delete(0);
        ctx.db.player_id_counter().insert(PlayerIdCounter {
            id: 0,
            next: allocated.saturating_add(1),
        });
        allocated
    } else {
        // Lazy seed — one full scan, paid exactly once after a fresh
        // deployment. Two constraints:
        //  - Must include existing players so the counter doesn't hand
        //    out ids that already exist (covers databases that pre-date
        //    the counter table or were migrated from an older schema).
        //  - Must start at least at `FIRST_PLAYER_ID` so the
        //    `0..FIRST_PLAYER_ID` reserved range stays free for
        //    system / pseudo-players (e.g., a world-owner for trees,
        //    rocks, etc.).
        // The `.max(FIRST_PLAYER_ID)` clamp picks whichever lower bound
        // is stricter — existing data wins if it ran past the reserve.
        let current_max = ctx
            .db
            .players()
            .iter()
            .map(|p| p.player_id)
            .max()
            .unwrap_or(0);
        let allocated = current_max.saturating_add(1).max(FIRST_PLAYER_ID);
        ctx.db.player_id_counter().insert(PlayerIdCounter {
            id: 0,
            next: allocated.saturating_add(1),
        });
        allocated
    }
}

/// Delete every version row for `player_id`, plus all sessions and any cards
/// the player owned or stashed. Routed through here so `resolve_caller`'s
/// invariant ("session.player_id always references a live player") never
/// breaks.
pub fn delete_player(ctx: &ReducerContext, player_id: u32) {
    let session_ids: Vec<Identity> = ctx
        .db
        .player_sessions()
        .player_id()
        .filter(player_id)
        .map(|s| s.identity)
        .collect();
    for identity in session_ids {
        ctx.db.player_sessions().identity().delete(identity);
    }

    // Cascade-delete every version row of every card this player owns.
    // `cards.owner_id` is not currently btree-indexed, so this is a single
    // O(N) scan over the full cards table — fine while the table is small.
    // If this becomes hot, add `#[index(btree)] pub owner_id: u32` on
    // `Card` and switch to `cards().owner_id().filter(player_id)`.
    //
    // The earlier "cards stashed in this player's inventory" cascade keyed
    // on `Card.macro_zone == player_id` is gone: in the current schema
    // `macro_zone` holds packed world coordinates (see
    // `packed::pack_macro_zone`), not a player_id, so that match no longer
    // means anything.
    let mut card_ids: BTreeSet<u32> = BTreeSet::new();
    for c in ctx.db.cards().iter() {
        if c.owner_id == player_id {
            card_ids.insert(c.card_id);
        }
    }
    for card_id in card_ids {
        let valid_ats: Vec<u64> = ctx
            .db
            .cards()
            .card_id()
            .filter(card_id)
            .map(|c| c.valid_at)
            .collect();
        for v in valid_ats {
            ctx.db.cards().valid_at().delete(v);
        }
    }

    // And every version row of the player itself.
    let valid_ats: Vec<u64> = ctx
        .db
        .players()
        .player_id()
        .filter(player_id)
        .map(|p| p.valid_at)
        .collect();
    for v in valid_ats {
        ctx.db.players().valid_at().delete(v);
    }
}

/// Spawn the soul card that represents a freshly-created player.
///
/// Today the only soul variant is `soul/default/human` — every new
/// player gets one. Placed at `(surface=0, macro_zone=0, micro_zone=0,
/// micro_location=0)` to mirror the Player row's "not yet placed"
/// initial location fields. Owned by the new player, so inventory
/// queries / hex-owner resolution treat the soul like any other
/// player-owned card.
///
/// Goes through the standard `cards::create` + `on_create::trigger`
/// path, so any future `on_create.self.human` recipe (e.g. one that
/// hands out starter discipline cards) wires up automatically.
fn spawn_soul_for(ctx: &ReducerContext, player_id: u32) -> Result<(), String> {
    let soul_def = find_packed_by_key("human")
        .map_err(|e| format!("spawn_soul_for: lookup human soul: {e}"))?
        .ok_or_else(|| "spawn_soul_for: human soul not registered".to_string())?;
    let soul_card_id = cards::next_card_id(ctx);
    cards::create(
        ctx,
        soul_card_id,
        /* surface         */ 0,
        /* macro_zone      */ 0,
        /* micro_zone      */ 0,
        /* micro_location  */ 0,
        /* owner_id        */ player_id,
        soul_def,
        /* flags           */ 0,
    );
    crate::on_create::trigger(ctx, soul_card_id, player_id)?;
    Ok(())
}

/// Trust-on-first-use registration / login.
///
/// If no `Player` exists with the given (case-sensitive) name, one is
/// created. Either way, a `PlayerSession` is established (or replaced) for
/// the caller's current `Identity`, mapping it to that `Player.player_id`.
///
/// **This is intentionally insecure.** Anyone can call `claim_or_login`
/// with any name and become that player — there is no password, token, or
/// external auth check. Replace this with token-based or external auth
/// before exposing the module to anyone you don't trust.
#[reducer]
pub fn claim_or_login(ctx: &ReducerContext, name: String) -> Result<(), String> {
    validate_player_name(&name)?;

    let player_id = match latest_by_name(ctx, &name) {
        Some(player) => {
            // Reserved-range players (`__world__`, future NPC owners,
            // etc.) live at `player_id < FIRST_PLAYER_ID`. They're
            // server-internal and must never be claimable by a human
            // — claiming would let a player drag-pick the world's
            // entire tree / rock inventory through normal inventory
            // ops. `next_player_id` already starts above the reserve,
            // so this branch is the only entry point that could resolve
            // to a reserved id (via name lookup of a server-seeded row).
            if player.player_id < FIRST_PLAYER_ID {
                return Err(format!(
                    "player name {:?} is reserved",
                    name
                ));
            }
            player.player_id
        }
        None => {
            let new_id = next_player_id(ctx);
            create(ctx, new_id, name, 0, 0, 0, 0);
            spawn_soul_for(ctx, new_id)?;
            new_id
        }
    };

    let sender = ctx.sender();
    ctx.db.player_sessions().identity().delete(sender);
    ctx.db.player_sessions().insert(PlayerSession {
        identity: sender,
        player_id,
    });

    Ok(())
}

/// Clean up the disconnecting connection's `PlayerSession` row.
///
/// SpacetimeDB calls this automatically on every client disconnect. Delete
/// is idempotent — if the connection never logged in, this is a no-op.
#[reducer(client_disconnected)]
pub fn client_disconnected(ctx: &ReducerContext) {
    let sender = ctx.sender();
    ctx.db.player_sessions().identity().delete(sender);
}
