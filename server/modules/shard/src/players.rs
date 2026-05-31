use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, Identity, ReducerContext, Table};
use std::collections::BTreeSet;

use crate::cards::{self, cards as _cards_table};
use crate::flags::state_flags;
use crate::packed::{
    loose_kind_for_surface, pack_macro_zone, pack_valid_at, valid_at_time,
};
use crate::sequence;
use crate::souls::{soul_privates as _soul_privates_table, with_portrait, SoulPrivate};

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
/// stored on the server — it's a purely client-side construct held
/// by `SoulManager`. Each
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
    /// Data-shard partition this row belongs to (`crate::DATA_SHARD`; `0` today).
    pub data_shard: u16,
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
    /// Free-form per-player flag bits. Public so other clients can
    /// read them when they need to render this player's owned
    /// surfaces (the faction subfield drives the object-texture
    /// pack picker; see [`PLAYER_FLAG_FACTION_SHIFT`] and the
    /// `objects/<faction>/<size>_<aspect>/` resolver). PlayerProfile
    /// would be the natural home for entitlement-style fields, but
    /// it's per-client subscribed — others can't see it, and
    /// other-player-owned art would render with the wrong faction.
    ///
    /// Bit layout:
    /// - bits 0..=1 — `faction` (u2, 4 values)
    /// - bits 2..=31 — reserved for future per-player toggles
    ///
    /// Catalog-style flag registry (mirroring `cards/flags.json`)
    /// can land once there are more fields to read by name; for
    /// today's single field, helpers below access bits directly.
    pub flags: u32,
}

/// Bit offset of the `faction` subfield inside [`Player::flags`].
/// 4 values total (`u2`); content semantics live client-side
/// today (`0 = neutral`, etc.) but the storage doesn't bake any
/// names — content can rename freely without a row migration.
pub const PLAYER_FLAG_FACTION_SHIFT: u32 = 0;
/// Mask for the `faction` subfield. Use as
/// `(player.flags >> PLAYER_FLAG_FACTION_SHIFT) & PLAYER_FLAG_FACTION_MASK`
/// to read.
pub const PLAYER_FLAG_FACTION_MASK: u32 = 0b11;

/// Extract the `faction` subfield from a player's `flags`. Returns
/// `0..=3`. Used by `claim_or_login` (default = 0) and any reducer
/// that gates on faction.
pub fn player_faction(player: &Player) -> u8 {
  ((player.flags >> PLAYER_FLAG_FACTION_SHIFT) & PLAYER_FLAG_FACTION_MASK) as u8
}

/// Re-pack a player's faction bits and write a new versioned row at
/// `time_ms`. Returns `Err` if no prior `Player` row exists for
/// `player_id`. The value is masked to the 2-bit slot — callers
/// passing `4..` lose the high bits silently (recipe authors are
/// expected to use the `Faction*` aliases in `recipes/aliases.json`).
///
/// Called by the recipe completion path (`action_completion::Effect::
/// SetPlayerFaction`) — recipes use `<owner-chain>.aspect.faction.set:
/// <int>` and the executor lands here. No direct reducer wraps this
/// today (faction is recipe-driven); add a `set_player_faction` reducer
/// here if a UI flow ever needs to call it outside the action system.
pub fn set_faction(
    ctx: &ReducerContext,
    player_id: u32,
    time_ms: u64,
    faction: u8,
) -> Result<(), String> {
    let faction_bits = (faction as u32) & PLAYER_FLAG_FACTION_MASK;
    let slot_mask = PLAYER_FLAG_FACTION_MASK << PLAYER_FLAG_FACTION_SHIFT;
    update_with_at(ctx, player_id, time_ms, |p| {
        p.flags = (p.flags & !slot_mask) | (faction_bits << PLAYER_FLAG_FACTION_SHIFT);
    })
    .map(|_| ())
    .ok_or_else(|| format!("set_faction: no player row for player_id {player_id}"))
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
/// alongside the soul spawn (`spawn_soul_for`).
#[spacetimedb::table(accessor = player_profiles, public)]
#[derive(Debug, Clone)]
pub struct PlayerProfile {
    #[primary_key]
    pub player_id: u32,
    /// Data-shard partition this row belongs to (`crate::DATA_SHARD`; `0` today).
    pub data_shard: u16,
    /// Number of active magnetic actions owned by this player —
    /// summary of the `lifecycle_pending` detail table, kept on the
    /// row so the block-check in `propose_action` doesn't need a
    /// separate query in the common case. Maintained by
    /// `cards::write_at` on the magnetic-install / dead-transition
    /// paths. `0` means "no active magnetic actions" and the
    /// `earliest_lifecycle_expires_ms` field is then meaningless.
    pub lifecycle_count: u32,
    /// Earliest `expires_at_ms` among this player's active magnetic
    /// actions, or `0` when `lifecycle_count == 0`. The block-check
    /// reads only this field to determine whether to engage the
    /// gate (`now_ms > earliest + GRACE`). Recomputed from the
    /// `lifecycle_pending` detail rows whenever an entry changes.
    pub earliest_lifecycle_expires_ms: u64,
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
fn write_at(ctx: &ReducerContext, mut player: Player, time_ms: u64) -> Player {
    // "Last write at this (player_id, time_ms) wins." See
    // `cards::write_at` for the full rationale — same-time writes
    // would otherwise accumulate distinct rows under the new
    // sequence-bearing PK.
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
    // No per-write delete schedule — see `crate::gc` for the
    // unified periodic sweep that handles prior-version reap.
    inserted
}

// Convenience: insert at the server's wall-clock `now`. Reducer
// callers should prefer `create_at` and pass `effective_now_ms`.
pub fn create(ctx: &ReducerContext, player_id: u32, name: String) -> Player {
    create_at(ctx, player_id, name, now_ms(ctx))
}

// Insert a brand-new player at the given `time_ms`. valid_at is
// computed from `time_ms`; any value passed in is overwritten.
pub fn create_at(
    ctx: &ReducerContext,
    player_id: u32,
    name: String,
    time_ms: u64,
) -> Player {
    write_at(
        ctx,
        Player {
            valid_at: 0,
            data_shard: crate::DATA_SHARD,
            player_id,
            name,
            // Seed at 0 — anything below `now - retention_window` is
            // interpreted by the client as "no recent session," so
            // the player's first login subscribes to no scrollback.
            last_login_secs: 0,
            // Faction = 0 (neutral). Future signup flows can pass
            // a chosen faction in here once the UI exists; today
            // every fresh account starts neutral and any later
            // mutation goes through a (TBD) `set_player_faction`
            // reducer that writes a new versioned row.
            flags: 0,
        },
        time_ms,
    )
}

// Pick up the latest row at-or-before `time_ms`, mutate it via `f`,
// write it back at `time_ms`. Returns None if no prior row exists.
// Mirrors `cards::update_with_at` — use this from reducers that have
// resolved an `effective_now_ms` value.
pub fn update_with_at<F>(
    ctx: &ReducerContext,
    player_id: u32,
    time_ms: u64,
    f: F,
) -> Option<Player>
where
    F: FnOnce(&mut Player),
{
    let mut p = ctx
        .db
        .players()
        .player_id()
        .filter(player_id)
        .filter(|p| valid_at_time(p.valid_at) <= time_ms)
        .max_by_key(|p| valid_at_time(p.valid_at))?;
    f(&mut p);
    Some(write_at(ctx, p, time_ms))
}

// Convenience wrapper: stamp at server wall-clock `now`. Mostly a
// holdover for callers outside the reducer-args-to-now_eff plumbing
// (e.g., test setup). New code in reducers should pass `time_ms`
// explicitly via `update_with_at`.
pub fn update_with<F>(ctx: &ReducerContext, player_id: u32, f: F) -> Option<Player>
where
    F: FnOnce(&mut Player),
{
    update_with_at(ctx, player_id, now_ms(ctx), f)
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

// ---- magnetic summary --------------------------------------------------

/// Re-summarize a player's magnetic-action state by walking the
/// `lifecycle_pending` detail table and writing the new
/// `(count, earliest_expires_ms)` pair onto their `PlayerProfile`.
/// No-op if the player has no profile row (shouldn't happen in
/// normal flow — every player_id with lifecycle_pending entries
/// came through `claim_or_login`'s profile-seed path).
///
/// Called from `cards::write_at` after magnetic-install /
/// dead-transition events. Idempotent: re-running yields the same
/// state for unchanged input.
pub fn resync_lifecycle_summary(ctx: &ReducerContext, player_id: u32) {
    if player_id == 0 {
        // World-owned magnetics — no profile to update.
        return;
    }
    let (count, earliest) = crate::lifecycle_pending::summarize_for_player(ctx, player_id);
    let Some(mut profile) = ctx.db.player_profiles().player_id().find(player_id) else {
        return;
    };
    if profile.lifecycle_count == count && profile.earliest_lifecycle_expires_ms == earliest {
        return;
    }
    profile.lifecycle_count = count;
    profile.earliest_lifecycle_expires_ms = earliest;
    // PlayerProfile is mutated in place (one row per player, not
    // history-style), so a delete+insert keyed by player_id is the
    // pattern.
    ctx.db.player_profiles().player_id().delete(player_id);
    ctx.db.player_profiles().insert(profile);
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
/// **Exempt from `effective_now_ms` grace check** — same rationale as
/// `claim_or_login`. The client also calls this reducer on every
/// shard reconnect to re-seed `noteServerTime` after a stale-capture
/// gap (the player row's update is delivered as a `Reducer`-tagged
/// row event because the client is already subscribed to the player
/// row from the prior session). At reconnect time the client's
/// offset window may be too stale to pass a grace check, so the
/// grace check is bypassed; the reducer's purpose is precisely to
/// repair that staleness, not to depend on it.
///
/// `_client_time_ms` is accepted but ignored, kept for wire-format
/// consistency.
///
/// No-op (returns `Ok`) if the player has no prior row, which can
/// happen mid-creation; the next login will land on a real row.
#[reducer]
pub fn set_last_login(ctx: &ReducerContext, _client_time_ms: u64) -> Result<(), String> {
    let player_id = resolve_caller(ctx)?;
    // Stamp at `ctx.timestamp − TIME_DRIFT_BUFFER_MS` so the row is
    // immediately visible to the client's buffered `serverNowMs()`
    // view — matches the same convention as `claim_or_login`. See
    // the doc on that reducer for the full rationale. The
    // `last_login_secs` value itself uses the shifted time too, so
    // the recorded "last login" is consistent with what the client's
    // chat-window calculator expects.
    let now_ms = now_ms(ctx).saturating_sub(crate::cards::TIME_DRIFT_BUFFER_MS);
    let now_secs = (now_ms / 1_000) as u32;
    update_with_at(ctx, player_id, now_ms, |p| {
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
pub fn claim_or_login(
    ctx: &ReducerContext,
    _client_time_ms: u64,
    name: String,
) -> Result<(), String> {
    validate_player_name(&name)?;
    // **Exempt from `effective_now_ms` grace check.** This is the
    // bootstrap reducer: the client's clock offset isn't yet
    // synchronized to the server's (the offset window is empty until
    // `captureReducerTimestamp` fires from a row event), so a grace
    // check on `client_time_ms` would systematically reject the first
    // call of every session whose host clock is off by more than 2N.
    //
    // Instead, this reducer writes the player row at
    // `ctx.timestamp − TIME_DRIFT_BUFFER_MS` — i.e. shifted backward
    // by the same amount the client offsets `serverNowMs()` by. This
    // is the moment the client's `serverNowMs()` will read *once the
    // window is seeded by this very row's delivery*. Stamping at the
    // raw `ctx.timestamp` would put the row N ms in the client's
    // future, forcing the player to wait N seconds before
    // `promote()` brings the row into `current` and `waitForPlayer`
    // resolves. Shifting backward by N makes the row visible on the
    // very next promote tick.
    //
    // The client subscribes to the player row BEFORE calling this
    // reducer (see `PlayerManager.claimOrLogin`), so the
    // transaction-update delivery carries the row as a `Reducer`-
    // tagged row event, which feeds `noteServerTime` and
    // synchronizes the offset window. From there on, every reducer is
    // submitted with a valid `client_time_ms`.
    //
    // `_client_time_ms` is accepted but ignored, kept for wire-format
    // consistency so the client's `ReducerManager.claimOrLogin`
    // wrapper doesn't need a special case.
    let now_ms = now_ms(ctx).saturating_sub(crate::cards::TIME_DRIFT_BUFFER_MS);

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
            // Signup creates only the player record + session. The
            // player-soul is no longer spawned here as a side-effect:
            // the client drives soul creation post-login via the
            // `spawn_soul` reducer (it subscribes its owned cards and,
            // finding none, requests one). There is no separate
            // character-creation step yet.
            create_at(ctx, new_id, name, now_ms);
            // Seed the per-player private state row.
            ctx.db.player_profiles().insert(PlayerProfile {
                player_id: new_id,
                data_shard: crate::DATA_SHARD,
                lifecycle_count: 0,
                earliest_lifecycle_expires_ms: 0,
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

    // Always stamp a fresh `last_login_secs` row, even on the
    // existing-player branch. Two reasons:
    //
    //   1. **Welcome-back stamp.** Players returning to the game
    //      naturally re-bump the timestamp; the client uses it to
    //      decide which chat scrollback window to subscribe to.
    //   2. **Clock-sync bootstrap.** This is the one row write the
    //      client *needs* to receive as a `Reducer`-tagged event so
    //      `noteServerTime` can seed the offset window. On the
    //      new-player branch `create_at` above already wrote a row; on
    //      the existing-player branch nothing else in the reducer
    //      writes to a subscribed table (`player_sessions` is
    //      server-private). Without this update, returning players
    //      would land with an empty offset window and the first
    //      subsequent reducer would fail the grace check.
    update_with_at(ctx, player_id, now_ms, |p| {
        p.last_login_secs = (now_ms / 1_000) as u32;
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

/// Surface the player's `player_soul` lives on. Deliberately `0` (not
/// the world band `64`) so the player-soul is never rendered anywhere
/// — it *is* the player, a thin soul that owns the world-facing souls
/// in its inventory rather than standing on the map itself.
const PLAYER_SOUL_SURFACE: u8 = 0;

/// Soul auto-granted to every new player. Hardcoded since the
/// starter-pack content layer was removed — a fresh account is always
/// a `player_soul`, resolved through the content catalog at signup.
const STARTER_SOUL_KEY: &str = "player_soul";

/// Count the distinct `player_soul` cards `player_id` directly owns —
/// cards whose latest version has `owner_id == player_id` and carries
/// `is_owned_by_player`. That flag means precisely "`owner_id` is a
/// player_id" (`content/cards/flags.json`), so it is the correct resolver
/// for the `card_id` / `player_id` numeric-namespace collision: a
/// world-facing soul owned by the player_soul *card* (whose card_id may
/// numerically equal the player_id) does NOT carry the flag and so is
/// excluded. Dedups across `valid_at` versions via the `owner_id` btree
/// index.
fn owned_soul_count(ctx: &ReducerContext, player_id: u32) -> u32 {
    let mask = state_flags().is_owned_by_player;
    let mut card_ids = BTreeSet::new();
    for row in ctx.db.cards().owner_id().filter(player_id) {
        card_ids.insert(row.card_id);
    }
    card_ids
        .into_iter()
        .filter(|&card_id| {
            cards::latest(ctx, card_id)
                .is_some_and(|c| c.owner_id == player_id && c.flags_state & mask != 0)
        })
        .count() as u32
}

/// Spawn the caller's `player_soul` on demand. The client drives soul
/// creation now: post-login it subscribes its owned cards (`owner_id ==
/// player_id`, filtered by `is_owned_by_player`) and, finding none,
/// calls this — rather than `claim_or_login` spawning the soul as a
/// signup side-effect.
///
/// **Idempotent under races.** The client passes `soul_index = 1 + (souls
/// it currently sees)`. If the player already owns at least `soul_index`
/// souls, the request raced an earlier spawn (or the client's local view
/// lagged the server) — we reject so a stale-low client count can never
/// double-spawn. With a single soul today `soul_index` is always 1, so
/// this rejects any second spawn outright; the parameter generalizes to
/// future multi-character signup.
///
/// **Authorization is the gateway's job.** This reducer trusts its
/// `player_id` argument; a real deployment routes the call through the
/// gateway, which has the cross-DB view to verify the caller's identity
/// maps to `player_id` in the auth DB. Dev clients call it directly.
/// (Mirrors the `cards` module's `spawn_soul` convention.)
///
/// Delegates to `spawn_soul_for`; the soul card write triggers
/// `souls::on_card_write`, which auto-creates the matching `Soul` row.
///
/// `_client_time_ms` is accepted but ignored, kept for wire-format
/// consistency with the other reducers.
#[reducer]
pub fn spawn_soul(
    ctx: &ReducerContext,
    _client_time_ms: u64,
    player_id: u32,
    soul_index: u32,
) -> Result<(), String> {
    let owned = owned_soul_count(ctx, player_id);
    if owned >= soul_index {
        return Err(format!(
            "spawn_soul: player {player_id} already owns {owned} soul(s) \
             (requested index {soul_index}) — rejected",
        ));
    }
    // Stamp at `now − TIME_DRIFT_BUFFER_MS` so the rows are immediately
    // visible to the client's buffered `serverNowMs()` view — matches
    // the `claim_or_login` / `set_last_login` convention.
    let time_ms = now_ms(ctx).saturating_sub(crate::cards::TIME_DRIFT_BUFFER_MS);
    spawn_soul_for(ctx, player_id, time_ms)
}

/// Grant a player their `player_soul`: spawn it on surface 0 (never
/// rendered — it *is* the player) and seed an empty `SoulPrivate`
/// row. Called from the `spawn_soul` reducer.
///
/// Deliberately minimal — the player-soul is a thin owner of other
/// souls. It gets no starter inventory and no starter blueprints;
/// those belong to the world-facing souls it will come to own.
///
/// The soul card write triggers `souls::on_card_write` branch (1),
/// which auto-creates the matching `Soul` row — so this fn never
/// touches the `Soul` table directly.
///
/// Failure propagates and rolls the whole signup transaction back.
fn spawn_soul_for(
    ctx: &ReducerContext,
    player_id: u32,
    time_ms: u64,
) -> Result<(), String> {
    let soul_def = find_packed_by_key(STARTER_SOUL_KEY)?.ok_or_else(|| {
        format!(
            "spawn_soul_for: soul def {:?} not in content catalog",
            STARTER_SOUL_KEY,
        )
    })?;
    let soul_card_id = cards::next_card_id(ctx);

    // Deterministic 4-bit portrait pick — mixing the soul's card id
    // with `time_ms` and `player_id` gives a stable per-soul value
    // without an rng (reducers must stay deterministic).
    let portrait_seed =
        (time_ms as u32) ^ (time_ms >> 32) as u32 ^ player_id ^ soul_card_id;
    let portrait_id = ((portrait_seed ^ (portrait_seed >> 4)) & 0xF) as u8;
    let soul_flags_state =
        with_portrait(state_flags().is_owned_by_player, portrait_id);

    cards::create_at(
        ctx,
        soul_card_id,
        time_ms,
        /* macro_zone      */ crate::packed::with_surface(pack_macro_zone(0, 0), PLAYER_SOUL_SURFACE),
        /* micro           */ cards::Micro::snap(0, 0, loose_kind_for_surface(PLAYER_SOUL_SURFACE)),
        /* owner_id        */ player_id,
        soul_def,
        /* flags_state     */ soul_flags_state,
        /* flags_bk        */ 0,
    );

    // Empty per-soul private state — no starter blueprints granted.
    ctx.db.soul_privates().insert(SoulPrivate {
        card_id: soul_card_id,
        data_shard: crate::DATA_SHARD,
        blueprints_0: 0,
        active_blueprints: 0,
    });

    // --- DEV TEST SEED: a world-facing "human" soul + a dust in its inventory.
    // Ownership chain: player -> player_soul -> human -> dust. The human stands
    // on the world at hex (0, 0); its inventory (surface = INVENTORY_LAYER,
    // owner = human card_id) holds one dust so the second (inventory) viewport
    // has something to render. Disposable pre-release seeding — remove once
    // real soul/inventory acquisition exists.
    let human_def = find_packed_by_key("human")?.ok_or_else(|| {
        "spawn_soul_for: \"human\" def not in content catalog".to_string()
    })?;
    let human_card_id = cards::next_card_id(ctx);
    let human_portrait_seed =
        (time_ms as u32) ^ (time_ms >> 32) as u32 ^ player_id ^ human_card_id;
    let human_portrait_id =
        ((human_portrait_seed ^ (human_portrait_seed >> 4)) & 0xF) as u8;
    // Portrait only — NO `is_owned_by_player`. The human's `owner_id` is
    // the player_soul *card* (below), not a player_id; the flag means
    // exactly "owner_id is a player_id", so setting it here would make
    // owner-walks (`owning_player`) and `owned_soul_count` mis-resolve the
    // human as a directly player-owned soul.
    let human_flags_state = with_portrait(0, human_portrait_id);
    cards::create_at(
        ctx,
        human_card_id,
        time_ms,
        /* macro_zone      */ crate::packed::pack_macro_zone_full(0, crate::packed::WORLD_LAYER, 0, 0),
        /* micro           */ cards::Micro::snap(0, 0, loose_kind_for_surface(crate::packed::WORLD_LAYER)),
        /* owner_id        */ soul_card_id,
        human_def,
        /* flags_state     */ human_flags_state,
        /* flags_bk        */ 0,
    );
    ctx.db.soul_privates().insert(SoulPrivate {
        card_id: human_card_id,
        data_shard: crate::DATA_SHARD,
        blueprints_0: 0,
        active_blueprints: 0,
    });

    // Seed the human's inventory as a soul-scoped Region instead of eagerly
    // creating the Zone: only the (0, 0) zone is present, none available. The
    // client's region gate requests that zone on demand when the inventory
    // viewport opens (same on-demand path as world zones), at which point
    // `request_zone` backs it with a single empty rect Zone (~153 bytes vs 64
    // empty tile cards). The dust card below still lands at the inventory
    // macro_zone and surfaces once that zone subscription opens.
    crate::regions::seed_soul_inventory_region(ctx, human_card_id, time_ms);

    let dust_def = find_packed_by_key("dust")?.ok_or_else(|| {
        "spawn_soul_for: \"dust\" def not in content catalog".to_string()
    })?;
    let dust_card_id = cards::next_card_id(ctx);
    cards::create_at(
        ctx,
        dust_card_id,
        time_ms,
        /* macro_zone      */ crate::packed::pack_macro_zone_full(human_card_id, crate::packed::INVENTORY_LAYER, 0, 0),
        /* micro           */ cards::Micro::snap(0, 0, loose_kind_for_surface(crate::packed::INVENTORY_LAYER)),
        /* owner_id        */ human_card_id,
        dust_def,
        /* flags_state     */ 0,
        /* flags_bk        */ 0,
    );

    Ok(())
}
