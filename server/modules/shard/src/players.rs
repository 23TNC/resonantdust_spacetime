use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, Identity, ReducerContext, Table};
use std::collections::BTreeSet;

use crate::cards::{self, cards as _cards_table};
use crate::flags::state_flags;
use crate::packed::{
    pack_macro_zone, pack_micro_zone, pack_nibbles, pack_valid_at, valid_at_time, StackedState,
    PLAYER_DIMENSION_LAYER, PLAYER_INVENTORY_LAYER,
};
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
    /// Bit field of discovered **player-scope** blueprints, ids
    /// 1..=64. Bit position is `id - 1`, matching the 1-indexed
    /// mapping in `content/player_blueprints/id.json`. Player-scope
    /// is distinct from soul-scope: soul blueprints live on
    /// `SoulPrivate.blueprints_0` with their own id namespace.
    /// `0` on signup — discovery is gameplay-driven. Flipping a
    /// bit on is one-way.
    pub blueprints_0: u64,
    /// Packed `[count: u4 | max: u4]` — placed `player_blueprint`
    /// cards owned by this player vs the cap. Use
    /// [`packed::pack_nibbles`] / [`packed::unpack_nibbles`] to
    /// access. Maintained by the `souls::on_card_write` hook the
    /// same way `lifecycle_count` is — a spawn / death of a
    /// player-blueprint card adjusts the `count` nibble. The
    /// `max` nibble is set on signup (default 1) and grown later
    /// via a (TBD) unlock mechanism. Cap reached → server's
    /// `request_player_blueprint` rejects.
    pub blueprint_info: u8,
    /// Packed `[count: u4 | max: u4]` — live soul cards owned by
    /// this player vs the cap. Replaces the old hardcoded
    /// `MAX_SOULS_PER_PLAYER = 5` constant: `count` is maintained
    /// by the `on_card_write` hook (incremented on soul-card
    /// create, decremented on soul-card dead); `max` defaults to
    /// 5 on signup and can be grown via gameplay. The
    /// character-creation reducer reads both nibbles to gate
    /// new-soul spawns.
    pub soul_info: u8,
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
    // pattern. Same as the existing starter_packs update path (if
    // one existed).
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
            // Character creation (via starter-pack redemption) is a
            // separate, explicit flow — a freshly-claimed player has
            // no souls until they pick one through
            // `CharacterSelectScene`'s create-character path.
            create_at(ctx, new_id, name, now_ms);
            // Seed the per-player private state row. `starter_packs
            // = 1` grants bit 0 (the `human` default pack) on signup
            // so the player can immediately redeem the starter
            // content.
            ctx.db.player_profiles().insert(PlayerProfile {
                player_id: new_id,
                starter_packs: 1,
                lifecycle_count: 0,
                earliest_lifecycle_expires_ms: 0,
                // No player-blueprints discovered on signup.
                blueprints_0: 0,
                // 1 placement slot to start, 0 in use. Grows via
                // (TBD) unlock mechanism.
                blueprint_info: pack_nibbles(0, 1),
                // 5-soul cap matches the legacy
                // `MAX_SOULS_PER_PLAYER = 5` constant. `count`
                // starts at 0 — the new-player branch doesn't spawn
                // a soul itself; that happens via
                // `create_character`.
                soul_info: pack_nibbles(0, 5),
            });
            // Eager-create the player's private pocket dimension —
            // a 2×2 grid of full-8×8 Zones on
            // `PLAYER_DIMENSION_LAYER (62)` seeded with a 7-cell
            // hex-ring-1 walkable cluster around local (3, 3) so
            // souls have somewhere to stand on entry. Souls visit
            // it via `enter_player_dimension`. Failure here
            // propagates and rolls the whole new-player txn back so
            // player + profile + dim Zones commit together or not
            // at all.
            crate::zones::create_player_dimension(ctx, new_id, now_ms)?;
            // Seed the player-wide inventory bucket with a single
            // `dust` card so the inventory UI has something to show
            // on first login. Cards on `PLAYER_INVENTORY_LAYER`
            // carry `owner_id = player_id` with
            // `FLAG_OWNED_BY_PLAYER` set — `owning_player` resolves
            // them in one hop without walking a soul chain.
            seed_player_inventory(ctx, new_id, now_ms)?;
            // Drop a couple of `corpse` cards onto random walkable
            // tiles in chunk (0, 0) of the freshly-created dim. They
            // share the inventory layer's player-owned flag pattern
            // but live on `PLAYER_DIMENSION_LAYER`.
            seed_player_dim_corpses(ctx, new_id, now_ms)?;
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

/// Seed a freshly-created player's account-wide inventory bucket.
/// Drops one of each starter card into
/// `(surface=PLAYER_INVENTORY_LAYER, macro_zone=player_id)` with
/// `owner_id = player_id` and `FLAG_OWNED_BY_PLAYER` set so
/// `owning_player` resolves them directly.
///
/// Called from `claim_or_login`'s new-player branch. Failure
/// propagates and rolls the whole signup transaction back — the
/// player either gets their full starter set or doesn't exist
/// at all.
///
/// Starter cards: `dust` (the player-scope currency / catch-all),
/// `food` (sustenance), `reliquary` (spiritual vessel). Add new
/// entries to the list below to expand the kit.
const STARTER_CARD_KEYS: &[&str] = &["dust", "food", "reliquary"];

fn seed_player_inventory(
    ctx: &ReducerContext,
    player_id: u32,
    time_ms: u64,
) -> Result<(), String> {
    for &key in STARTER_CARD_KEYS {
        let packed = find_packed_by_key(key)?
            .ok_or_else(|| format!(
                "seed_player_inventory: card def {:?} not in content catalog",
                key,
            ))?;
        let card_id = cards::next_card_id(ctx);
        cards::create_at(
            ctx,
            card_id,
            time_ms,
            /* surface         */ PLAYER_INVENTORY_LAYER,
            /* macro_zone      */ player_id,
            /* micro_zone      */ 0,
            /* micro_location  */ 0,
            /* owner_id        */ player_id,
            packed,
            /* flags_state     */ state_flags().is_owned_by_player,
            /* flags_bk        */ 0,
        );
    }
    Ok(())
}

/// Drop a handful of `corpse` cards into the player's pocket dim on
/// pseudo-random tiles. Called from `claim_or_login`'s new-player
/// branch right after `create_player_dimension` lays down the dim's
/// seed tiles. Cells are picked from the same hex-disc cluster the
/// dim seeds — `concrete` ring tiles around the central `alter` — so
/// every corpse lands on something walkable. Deterministic in
/// `player_id` so the same player sees the same layout each login
/// (and different players see different layouts).
const SEED_RESONANCE_CORPSE_COUNT: usize = 2;
const SEED_CHORD_CORPSE_COUNT: usize = 1;
const SEED_CORPSE_COUNT: usize = SEED_RESONANCE_CORPSE_COUNT + SEED_CHORD_CORPSE_COUNT;

fn seed_player_dim_corpses(
    ctx: &ReducerContext,
    player_id: u32,
    time_ms: u64,
) -> Result<(), String> {
    // Faction-flavoured corpses — each variant's `<faction>` feature
    // locks its art to that faction's folder regardless of who's
    // looking. Two resonance + one chord today; swap for
    // `corpse_bio` / `corpse_relic` / generic `corpse` (player-
    // faction-tinted) as the seed content evolves.
    let resonance_packed = find_packed_by_key("corpse_resonance")?.ok_or_else(|| {
        "seed_player_dim_corpses: card def \"corpse_resonance\" not in content catalog".to_string()
    })?;
    let chord_packed = find_packed_by_key("corpse_chord")?.ok_or_else(|| {
        "seed_player_dim_corpses: card def \"corpse_chord\" not in content catalog".to_string()
    })?;

    let positions = walkable_dim_cells();
    let picks = pseudo_random_picks(&positions, SEED_CORPSE_COUNT, player_id);

    // Chunk (0, 0) — the first chunk of the player's 2×2 dim. The
    // seed disc is identical across all 4 chunks, but we drop the
    // corpses in one chunk so the player finds them in the same
    // place they spawn.
    let macro_zone = pack_macro_zone(0, 0);
    for (i, (local_q, local_r)) in picks.into_iter().enumerate() {
        let corpse_packed = if i < SEED_RESONANCE_CORPSE_COUNT {
            resonance_packed
        } else {
            chord_packed
        };
        let card_id = cards::next_card_id(ctx);
        let micro_zone = pack_micro_zone(local_q, local_r, StackedState::Free);
        cards::create_at(
            ctx,
            card_id,
            time_ms,
            /* surface         */ PLAYER_DIMENSION_LAYER,
            macro_zone,
            micro_zone,
            /* micro_location  */ 0,
            /* owner_id        */ player_id,
            corpse_packed,
            /* flags_state     */ state_flags().is_owned_by_player,
            /* flags_bk        */ 0,
        );
    }
    Ok(())
}

/// The concrete-tile cells in any player-dim chunk — the ring around
/// the central non-concrete fixtures. Mirrors the seed pattern in
/// `zones::create_player_dimension`: a hex disc of radius 2 around
/// local (3, 3) MINUS every fixture cell (alter, fountains, tables)
/// so corpses don't share a tile with one. Keep `NON_CONCRETE` in
/// sync with the `match (dq, dr)` arm in
/// `zones::build_dim_tiles` whenever a fixture moves or a new one
/// joins the seed layout.
fn walkable_dim_cells() -> Vec<(u8, u8)> {
    const CENTER: (i8, i8) = (3, 3);
    const RADIUS: i8 = 2;
    // (dq, dr) offsets from CENTER that hold non-concrete tiles in
    // the seed layout — must mirror `zones::build_dim_tiles`'s
    // `match (dq, dr)` arm.
    //
    //   (1, -2)  alter
    //   (2,  0)  anima fountain
    //   (-2, 0)  aether fountain
    //   (0,  1)  table SE
    //   (-1, 1)  table SW
    const NON_CONCRETE: &[(i8, i8)] = &[
        (1, -2),
        (2, 0),
        (-2, 0),
        (0, 1),
        (-1, 1),
    ];
    let mut out = Vec::new();
    for dq in -RADIUS..=RADIUS {
        for dr in -RADIUS..=RADIUS {
            let dist = (dq.abs() + dr.abs() + (dq + dr).abs()) / 2;
            if dist > RADIUS {
                continue;
            }
            if NON_CONCRETE.contains(&(dq, dr)) {
                continue;
            }
            let q = CENTER.0 + dq;
            let r = CENTER.1 + dr;
            if !(0..8).contains(&q) || !(0..8).contains(&r) {
                continue;
            }
            out.push((q as u8, r as u8));
        }
    }
    out
}

/// Pick `n` distinct entries from `positions` deterministically from
/// `seed`. Fisher-Yates partial shuffle backed by a small LCG —
/// enough randomness for "drop a few cards somewhere", not for
/// anything security-sensitive. Returns at most `positions.len()`
/// entries if `n` exceeds the pool.
fn pseudo_random_picks<T: Copy>(positions: &[T], n: usize, seed: u32) -> Vec<T> {
    let take = n.min(positions.len());
    let mut indices: Vec<usize> = (0..positions.len()).collect();
    let mut state: u32 = seed.wrapping_mul(0x9E37_79B9).wrapping_add(0x1234_5678);
    for i in (1..indices.len()).rev() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let j = (state as usize) % (i + 1);
        indices.swap(i, j);
    }
    indices.into_iter().take(take).map(|i| positions[i]).collect()
}
