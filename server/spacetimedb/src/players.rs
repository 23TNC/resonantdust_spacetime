use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, Identity, ReducerContext, Table};
use std::collections::BTreeSet;

use crate::cards::{self, cards as _cards_table};
use crate::packed::{pack_macro_zone, pack_micro_zone, pack_valid_at, valid_at_time, StackedState};
use crate::sequence;

/// World-layer surface threshold (mirrors the constant in
/// `action_completion.rs` / `actions.rs`). Souls spawn on this surface
/// so they live in the world tier rather than inventory.
const WORLD_LAYER: u8 = 64;

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
    /// Packed primary key — `[time_ms: u48 | seq: u16]` (high | low).
    /// `player_id` is on the row column (see below). Multiple rows per
    /// `player_id` form a version history; the latest is the one with
    /// the largest `valid_at_time`.
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
    /// `card_id` of the soul card this player currently inhabits — the
    /// in-world avatar that carries positional data (surface, macro_zone,
    /// micro_zone, micro_location). `0` before the soul has been spawned
    /// (a lazy migration in `claim_or_login` provisions one on next login
    /// for any player whose row predates the soul system). Replaces the
    /// older "positional fields directly on Player" model — every piece of
    /// in-world state now lives on a `Card`, and the player row is a thin
    /// identity row that points at *which* card the player is.
    ///
    /// Future multi-character support: this field names the *currently
    /// controlled* soul. Switching characters means swapping this id;
    /// other souls belonging to the same player would be normal owned
    /// cards. Today there's exactly one soul per player.
    pub soul_card_id: u32,
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

fn now_ms(ctx: &ReducerContext) -> u64 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64
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
    // "Last write at this (player_id, time_ms) wins." See
    // `cards::write_at` for the full rationale — same-time writes
    // would otherwise accumulate distinct rows under the new
    // sequence-bearing PK.
    let time_ms = now_ms(ctx);
    let stale: Vec<u64> = ctx
        .db
        .players()
        .player_id()
        .filter(player.player_id)
        .filter(|p| valid_at_time(p.valid_at) == time_ms)
        .map(|p| p.valid_at)
        .collect();
    for v in stale {
        ctx.db.players().valid_at().delete(v);
    }
    player.valid_at = pack_valid_at(time_ms, sequence::next_sequence(ctx));
    let inserted = ctx.db.players().insert(player);
    crate::schedule_delete_players::enqueue(ctx, inserted.player_id, inserted.valid_at);
    inserted
}

// Insert a brand-new player. valid_at is computed; pass 0 will be overwritten.
pub fn create(
    ctx: &ReducerContext,
    player_id: u32,
    name: String,
    soul_card_id: u32,
) -> Player {
    write(
        ctx,
        Player {
            valid_at: 0,
            player_id,
            name,
            soul_card_id,
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

pub fn set_soul_card_id(
    ctx: &ReducerContext,
    player_id: u32,
    soul_card_id: u32,
) -> Option<Player> {
    update_with(ctx, player_id, |p| p.soul_card_id = soul_card_id)
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

/// Spawn the soul card that represents a freshly-created player and
/// return its `card_id`.
///
/// Today the only soul variant is `soul/default/human` — every new
/// player gets one. Placed on the world surface at tile `(0, 0)` as a
/// state-3 OnHex virtual-hex-root (no parent hex card row at the
/// spawn point — the rect card sits on the bare world tile). Owned by
/// the new player, so inventory queries / hex-owner resolution treat
/// the soul like any other player-owned card.
///
/// Eventually the spawn location should be map-driven (a designated
/// spawn tile per region). For now it's fixed at world origin to keep
/// the bootstrap simple.
///
/// Goes through the standard `cards::create` + `on_create::trigger`
/// path, so any future `on_create.self.human` recipe (e.g. one that
/// hands out starter discipline cards) wires up automatically.
fn spawn_soul_for(ctx: &ReducerContext, player_id: u32) -> Result<u32, String> {
    let soul_def = find_packed_by_key("human")
        .map_err(|e| format!("spawn_soul_for: lookup human soul: {e}"))?
        .ok_or_else(|| "spawn_soul_for: human soul not registered".to_string())?;
    let soul_card_id = cards::next_card_id(ctx);
    let spawn_macro_zone = pack_macro_zone(0, 0);
    let spawn_micro_zone = pack_micro_zone(0, 0, StackedState::OnHex);
    cards::create(
        ctx,
        soul_card_id,
        /* surface         */ WORLD_LAYER,
        /* macro_zone      */ spawn_macro_zone,
        /* micro_zone      */ spawn_micro_zone,
        /* micro_location  */ 0,
        /* owner_id        */ player_id,
        soul_def,
        /* flags           */ 0,
    );
    crate::on_create::trigger(ctx, soul_card_id, player_id, now_ms(ctx))?;
    Ok(soul_card_id)
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
            // Lazy soul migration. Pre-soul-system players have
            // `soul_card_id = 0`; spawn one on next login and write
            // the id back. New cards will rarely take this branch
            // (they get a soul at creation below) but it's also the
            // recovery path if a player row exists without a
            // corresponding soul card for any reason (orphaned by a
            // bug, manual ops, etc.).
            if player.soul_card_id == 0 {
                let soul_card_id = spawn_soul_for(ctx, player.player_id)?;
                set_soul_card_id(ctx, player.player_id, soul_card_id);
            }
            player.player_id
        }
        None => {
            let new_id = next_player_id(ctx);
            // Spawn the soul first so we can record its id on the
            // player's first row, instead of writing the player twice
            // (once with `soul_card_id = 0`, then again after the
            // soul exists). Single write keeps the version history
            // clean.
            let soul_card_id = spawn_soul_for(ctx, new_id)?;
            create(ctx, new_id, name, soul_card_id);
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
