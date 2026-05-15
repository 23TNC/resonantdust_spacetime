use spacetimedb::{reducer, Identity, ReducerContext, Table};
use std::collections::BTreeSet;

use crate::cards::{self, cards as _cards_table};
use crate::packed::{pack_valid_at, valid_at_time};
use crate::sequence;

/// Maximum byte length of a `Player.name`. Enforced by `validate_player_name`
/// on the input name and again after normalization in `claim_or_login`.
pub const MAX_PLAYER_NAME_LEN: usize = 64;

/// First `player_id` `next_player_id` will hand out on a fresh deployment.
/// Ids `0..FIRST_PLAYER_ID` are reserved for system / pseudo-players —
/// e.g., a "world" player that owns trees, rocks, and other unowned-by-
/// any-human world cards. Real players coming through `claim_or_login`
/// start at `FIRST_PLAYER_ID` and go up.
pub const FIRST_PLAYER_ID: u32 = 1024;

/// Public player identity row. Other clients mirror this table for
/// name lookups; keep it narrow so per-player private state
/// (entitlements, settings) lives in `PlayerProfile` instead.
///
/// "Which soul is this player currently controlling" is **not**
/// stored on the server — it's a purely client-side construct
/// driven by `CharacterSelectScene` and held by `SoulManager`. Each
/// reducer that needs a soul takes `soul_card_id` explicitly (or
/// derives one via `cards::owning_soul` from a card already in
/// context). Souls themselves live as cards with
/// `FLAG_OWNED_BY_PLAYER` set and `owner_id = player_id`, so
/// "what souls does this player own" is `cards.owner_id().filter(player_id)`.
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
    /// Unix seconds at which this player most recently called
    /// `set_last_login`. `0` on a brand-new player (`create()` seeds
    /// it that way) until they finish their first login round-trip.
    ///
    /// Read by clients to decide the chat-subscription threshold: if
    /// this value is within the chat retention window (e.g. one hour),
    /// the client subscribes to messages since this timestamp,
    /// catching up on what was said while they were away. Otherwise
    /// they subscribe only to messages from the current login forward.
    ///
    /// The client updates this *after* installing its chat
    /// subscription — see `set_last_login`. Best-effort: a crash
    /// between read and write leaves the field stale, which just
    /// means the next login replays the same window.
    pub last_login_secs: u32,
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

/// Per-player private state — the stuff the local player needs but
/// other players don't (entitlements, counters, settings). Kept off
/// the public `Player` row so other clients mirroring the player
/// table for name lookups don't pull in unlock bits etc. with it.
///
/// **Subscription pattern.** Public table, but each client only
/// subscribes to their *own* row via
/// `WHERE player_id = <caller's player_id>`. Server can't enforce
/// "no peeking at others" today — for low-sensitivity entitlement
/// data this is fine. Sensitive future fields should move to a
/// reducer-only path.
///
/// **Flat row, not history.** Unlike `Player` / `Card` / `Zone`,
/// this table has one row per `player_id` and is updated in place.
/// Profile state isn't time-stamped — there are no "what did the
/// player have unlocked at time T" reads downstream — so the
/// `valid_at` history machinery would be deadweight.
///
/// **Initial row.** Created in `claim_or_login`'s new-player branch
/// alongside the soul spawn. Default `starter_packs = 1` grants the
/// human starter pack (bit 0) on signup.
#[spacetimedb::table(accessor = player_profiles, public)]
#[derive(Debug, Clone)]
pub struct PlayerProfile {
    #[primary_key]
    pub player_id: u32,
    /// Bit field of unlocked starter packs. Bit position is assigned
    /// by `content/starter_packs/id.json` (matching the bare-key →
    /// id mapping used elsewhere). Bit 0 (= `human`) is set on
    /// signup so a fresh player can redeem the default pack
    /// immediately. Toggling a bit on (= unlock) is one-way under
    /// today's rules; "removing" a pack would require a tombstone
    /// policy we don't have yet.
    pub starter_packs: u64,
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
) -> Player {
    write(
        ctx,
        Player {
            valid_at: 0,
            player_id,
            name,
            // Seed at 0 — anything below `now - retention_window` is
            // interpreted by the client as "no recent session," so
            // the player's first login subscribes to no scrollback.
            last_login_secs: 0,
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

    // Cascade-delete every version row of every card this player owns
    // — including cards owned transitively via the soul's inventory
    // bucket. Under the post-flag-20 card-owner model, only soul cards
    // have `owner_id == player_id` directly; inventory cards point at
    // the soul's card_id, items in containers point at the container,
    // etc. We resolve "ultimately owned by player_id" via the
    // `cards::owning_player` walker, which climbs `owner_id` until it
    // hits the soul boundary.
    //
    // O(N walks) over the full cards table — fine while the table is
    // small. The earlier `Card.macro_zone == player_id` cascade is gone
    // (macro_zone now holds the soul's card_id for inventory cards, not
    // a player_id; the walker covers that case correctly).
    let mut card_ids: BTreeSet<u32> = BTreeSet::new();
    for c in ctx.db.cards().iter() {
        if card_ids.contains(&c.card_id) {
            continue;
        }
        if cards::owning_player(ctx, c.card_id) == Some(player_id) {
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


/// Stamp `last_login_secs` with the server's current wall-clock time
/// for the caller's player. Called by the client *after* it has read
/// the previous `last_login_secs` and installed its chat subscription
/// — see the field doc on `Player.last_login_secs`. Idempotent in the
/// sense that repeated calls just keep bumping the timestamp.
///
/// `player_id` is resolved server-side via `resolve_caller` — same
/// auth pattern as `moveSoul` / `equipCard` / `proposeAction`.
///
/// No-op (returns `Ok`) if the player has no prior row, which can
/// happen mid-creation; the next login will land on a real row.
#[reducer]
pub fn set_last_login(ctx: &ReducerContext) -> Result<(), String> {
    let player_id = resolve_caller(ctx)?;
    let now_secs = (now_ms(ctx) / 1_000) as u32;
    update_with(ctx, player_id, |p| {
        p.last_login_secs = now_secs;
    });
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
            // Character creation (via starter-pack redemption) is a
            // separate, explicit flow — a freshly-claimed player has
            // no souls until they pick one through
            // `CharacterSelectScene`'s create-character path.
            create(ctx, new_id, name);
            // Seed the per-player private state row. `starter_packs
            // = 1` grants bit 0 (the `human` default pack) on signup
            // so the player can immediately redeem the starter
            // content.
            ctx.db.player_profiles().insert(PlayerProfile {
                player_id: new_id,
                starter_packs: 1,
            });
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
