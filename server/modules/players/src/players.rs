use spacetimedb::{reducer, ReducerContext, Table};

use crate::packed::{pack_valid_at, valid_at_time};
use crate::sequence;

/// Client-server time-drift tolerance, mirrored from the `cards` module's
/// `cards::TIME_DRIFT_BUFFER_MS` — the steady-state forward-grace ceiling
/// (how far ahead of server a `client_time_ms` may be before rejection). Kept
/// as a local copy because the `players` and `cards` modules are separate
/// crates with no shared runtime dependency — keep the two in sync.
///
/// NOTE: this is *not* the player-row back-stamp depth — that's
/// `CLIENT_RENDER_BUFFER_MAX_MS` below. The two used to be the same value
/// (2s), which is why login rows landed in the client's future once the
/// render buffer grew past 2s.
pub const TIME_DRIFT_BUFFER_MS: u64 = 2_000;

/// Bootstrap back-stamp for the login row. The client renders behind true
/// server time by `clientDelay` — adaptive in `[1.5s, 5s]`, init 3s (see
/// `ReducerManager.CLIENT_DELAY_*`), which is DEEPER than
/// `TIME_DRIFT_BUFFER_MS` (2s, the steady-state forward-grace ceiling). At
/// login the client's clock window isn't seeded yet, so we can't stamp at its
/// actual buffered clock (`effective_now_ms` would reject the un-synced
/// `client_time_ms`). Instead back-stamp by the client's MAX render buffer so
/// the row is visible on the next promote tick wherever `clientDelay` settles —
/// stamping at only `now − 2s` left the row ~1s (up to 3s) in the client's
/// future, which is the login lag this fixes. Login rows carry no timing
/// semantics, so over-back-stamping is free. Mirror of the client's
/// `CLIENT_DELAY_MAX_MS`.
pub const CLIENT_RENDER_BUFFER_MAX_MS: u64 = 5_000;

/// The card shard a freshly-claimed player is assigned to. `0` while a
/// single `cards` database serves everyone — which it can, since the
/// auth DB is low-write and the hot card state is what actually needs
/// sharding. When that changes, replace this constant with a real
/// assignment policy (round-robin over live shards, least-loaded, etc.)
/// and stamp the result onto `Player.data_shard`.
const CARD_SHARD: u16 = 0;

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
    /// The **card shard** this player is assigned to — the `data_shard`
    /// partition whose `cards`-module database holds this player's cards
    /// and souls. The client reads this at login to know which card
    /// database to subscribe to. `0` today (single card shard); the
    /// assignment policy that distributes players across shards lands
    /// here when sharding actually splits.
    ///
    /// No soul id is stored here. After connecting to the card shard the
    /// client finds its soul(s) directly — `cards.owner_id().filter(player_id)`
    /// returns the player's top-level `player_soul` cards (a player can own
    /// more than one; that's the future multi-character handle). If the
    /// query is empty the client calls the card shard's `spawn_soul` to
    /// mint one. These top-level player-souls are never rendered in the world.
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
    /// - bits 0..=1   — `faction` (u2) — **DEPRECATED**: faction moves to the
    ///   soul; bits reclaimable once the faction→soul migration lands.
    /// - bits 8..=15  — `permissions` capability byte (see `PERM_*` /
    ///   [`PLAYER_FLAG_PERMS_SHIFT`]). Authoritative for entitlement checks.
    /// - bits 2..=7, 16..=31 — reserved for future per-player toggles
    ///
    /// Catalog-style flag registry (mirroring `cards/flags.json`)
    /// can land once there are more fields to read by name; for
    /// today's small set, helpers below access bits directly.
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

// ---- permissions -------------------------------------------------------
//
// A player's entitlements live in the `permissions` capability byte of
// `Player.flags` (bits 8..=15). The check is flag-based and authoritative:
// the `0..FIRST_PLAYER_ID` reserved id range is an *allocation* convention for
// system / developer accounts (the accounts you'd grant capabilities to), not
// the check itself. Capabilities compose — a granted set is the OR of `PERM_*`.

/// Bit offset of the permissions capability byte inside [`Player::flags`].
/// Disjoint from the (deprecated) faction subfield so both coexist through the
/// faction→soul migration that will later reclaim bits 0..=1.
pub const PLAYER_FLAG_PERMS_SHIFT: u32 = 8;
/// Mask for the permissions byte (8 capability bits).
pub const PLAYER_FLAG_PERMS_MASK: u32 = 0xFF;

/// May add or modify DSL content at runtime (`add_content` / `modify_content`).
pub const PERM_CONTENT_AUTHOR: u8 = 1 << 0;
// reserved capability bits: 1<<1 world-admin, 1<<2 player-admin, …

/// The player's granted capability set (the permissions byte of `flags`).
pub fn player_perms(player: &Player) -> u8 {
    ((player.flags >> PLAYER_FLAG_PERMS_SHIFT) & PLAYER_FLAG_PERMS_MASK) as u8
}

/// True iff the player holds **every** capability in `caps` (an OR of `PERM_*`).
pub fn player_has(player: &Player, caps: u8) -> bool {
    player_perms(player) & caps == caps
}

/// Re-pack a player's permissions byte and write a new versioned row at
/// `time_ms`. Returns `Err` if no prior `Player` row exists. Granting is itself
/// privileged — the caller (the gate) enforces who may invoke this; pre-release,
/// dev/system accounts in `0..FIRST_PLAYER_ID` are provisioned out-of-band.
pub fn set_permissions(
    ctx: &ReducerContext,
    player_id: u32,
    time_ms: u64,
    perms: u8,
) -> Result<(), String> {
    let slot_mask = PLAYER_FLAG_PERMS_MASK << PLAYER_FLAG_PERMS_SHIFT;
    update_with_at(ctx, player_id, time_ms, |p| {
        p.flags = (p.flags & !slot_mask) | ((perms as u32) << PLAYER_FLAG_PERMS_SHIFT);
    })
    .map(|_| ())
    .ok_or_else(|| format!("set_permissions: no player row for player_id {player_id}"))
}

// (PlayerSession / Identity-keyed sessions removed — the GATE now owns the
// WS → player_id session map. The players reducers are gate-mediated: they take
// an explicit `player_id` the gate supplies, mirroring how the cards reducers
// already trust the gate's `caller_player_id`. The gate is the auth boundary.)

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
// Assigns the default card shard.
pub fn create(ctx: &ReducerContext, player_id: u32, name: String) -> Player {
    create_at(ctx, player_id, name, CARD_SHARD, now_ms(ctx))
}

// Insert a brand-new player at the given `time_ms`. valid_at is
// computed from `time_ms`; any value passed in is overwritten.
// `data_shard` is the card shard this player is assigned to — see the
// `Player` struct docs.
pub fn create_at(
    ctx: &ReducerContext,
    player_id: u32,
    name: String,
    data_shard: u16,
    time_ms: u64,
) -> Player {
    write_at(
        ctx,
        Player {
            valid_at: 0,
            data_shard,
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

/// Delete every version row for `player_id`.
///
/// **Cards/souls are NOT cascaded here** — they live in the player's
/// assigned `cards` shard (a separate database this module can't write
/// to). Reaping a deleted player's cards is the card shard's job: its
/// GC sweep reaps world-/owner-dead rows, and a dedicated card-side
/// purge reducer (gateway-driven) can hard-delete on account removal.
pub fn delete_player(ctx: &ReducerContext, player_id: u32) {
    // Every version row of the player itself.
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
/// `player_id` is supplied by the gate (which owns the session) — same
/// gate-mediated auth pattern as the cards reducers' `caller_player_id`.
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
pub fn set_last_login(
    ctx: &ReducerContext,
    // Named without a leading underscore: SpacetimeDB's `/call` keys args on the
    // exact Rust param name, so `_client_time_ms` would be unaddressable by the
    // gate relay. Accepted but unused (the reducer stamps at server-now).
    client_time_ms: u64,
    player_id: u32,
) -> Result<(), String> {
    let _ = client_time_ms;
    // Stamp at `ctx.timestamp − CLIENT_RENDER_BUFFER_MAX_MS` so the row is
    // immediately visible to the client's buffered `serverNowMs()` view —
    // matches the same convention (and back-stamp depth) as `claim_or_login`;
    // see the constant's doc for why the shift tracks the client's MAX render
    // buffer rather than `TIME_DRIFT_BUFFER_MS`. The `last_login_secs` value
    // itself uses the shifted time too, so the recorded "last login" is
    // consistent with what the client's chat-window calculator expects.
    let now_ms = now_ms(ctx).saturating_sub(CLIENT_RENDER_BUFFER_MAX_MS);
    let now_secs = (now_ms / 1_000) as u32;
    update_with_at(ctx, player_id, now_ms, |p| {
        p.last_login_secs = now_secs;
    });
    Ok(())
}


/// Trust-on-first-use registration / login.
///
/// If no `Player` exists with the given (case-sensitive) name, one is
/// created (with a `data_shard` + profile). Either way the player row's
/// `last_login_secs` is bumped. The **gate** reads the resulting `player_id`
/// off the player row (looked up by name) and records the WS → player_id
/// session itself — this reducer no longer establishes an Identity session.
///
/// **This is intentionally insecure.** Anyone can call `claim_or_login`
/// with any name and become that player — there is no password, token, or
/// external auth check. Replace this with token-based or external auth
/// before exposing the module to anyone you don't trust.
#[reducer]
pub fn claim_or_login(
    ctx: &ReducerContext,
    // No leading underscore — `/call` keys on the exact param name, so the gate
    // relay must be able to address it. Accepted but unused (server-now stamp).
    client_time_ms: u64,
    name: String,
) -> Result<(), String> {
    let _ = client_time_ms;
    validate_player_name(&name)?;
    // **Exempt from `effective_now_ms` grace check.** This is the
    // bootstrap reducer: the client's clock offset isn't yet
    // synchronized to the server's (the offset window is empty until
    // `captureReducerTimestamp` fires from a row event), so a grace
    // check on `client_time_ms` would systematically reject the first
    // call of every session whose host clock is off by more than 2N.
    //
    // Instead, this reducer writes the player row at
    // `ctx.timestamp − CLIENT_RENDER_BUFFER_MAX_MS` — shifted backward
    // by the client's MAX render buffer (`clientDelay` ∈ [1.5s, 5s]).
    // Stamping at the raw `ctx.timestamp` would put the row N ms in the
    // client's future, forcing the player to wait N seconds before
    // `promote()` brings the row into `current` and `waitForPlayer`
    // resolves. Shifting back by the max buffer makes the row visible on
    // the very next promote tick regardless of where `clientDelay`
    // settles — the old `TIME_DRIFT_BUFFER_MS` (2s) shift was shallower
    // than the live buffer (3–5s) and left the player waiting ~1–3s.
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
    let now_ms = now_ms(ctx).saturating_sub(CLIENT_RENDER_BUFFER_MAX_MS);

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
            // Assign the player to a card shard. No soul is created
            // here — souls live in the assigned `cards` database, which
            // this module can't write to. After reading `data_shard`
            // off this row the client connects to that card shard,
            // looks up `cards.owner_id == player_id`, and calls
            // `spawn_soul(player_id)` there if it owns none yet.
            create_at(ctx, new_id, name, CARD_SHARD, now_ms);
            // Seed the per-player private state row.
            ctx.db.player_profiles().insert(PlayerProfile {
                player_id: new_id,
                data_shard: crate::DATA_SHARD,
            });
            new_id
        }
    };

    // The gate establishes the WS → player_id session (it reads this
    // player_id off the subscribed player row, looked up by name); the
    // players module no longer tracks an Identity-keyed session.

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

// (client_disconnected removed — there's no Identity-keyed session to reap.
// The gate owns the WS → player_id map and drops it when the client's WS closes;
// gate sessions are ephemeral, reconstructed on reconnect from shard truth.)
