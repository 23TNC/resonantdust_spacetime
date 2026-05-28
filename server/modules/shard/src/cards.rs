use resonantdust_content::definition_core::decode_definition;
use spacetimedb::{table, ReducerContext, Table};

use crate::flags::{bk_flags, state_flags};
use crate::lifecycle_pending::lifecycle_pending;
use crate::packed::{
    micro_zone_direction, pack_definition, pack_micro_zone, pack_stack_micro_zone,
    pack_valid_at, unpack_definition, unpack_micro_zone, unpack_zone_definition, valid_at_time,
    StackedState, STACK_DIR_HEX,
};
use crate::sequence;

/// Reserved sentinel player_id meaning "the world" (no real player
/// owns this). Server players `0..=1023` are reserved for pseudo-
/// owners; world-owned cards have `owner_id == 0` AND
/// `flags_state.is_owned_by_player` clear. [`owning_player`] returns
/// this when the walk terminates at `owner_id == 0`.
pub const WORLD_PLAYER_ID: u32 = 0;

/// Max steps [`owning_player`] / [`would_cycle`] will follow before
/// giving up. Defends against malformed cycles that slipped past the
/// writer-side check. In practice the chain is ~2 hops (apple → chest
/// → soul → player) so 32 is comfortable slack.
const OWNER_WALK_DEPTH_CAP: u32 = 32;

/// Define the pure-transform quartet (`<name>`, `write_<name>`,
/// `increment_<name>`, `decrement_<name>`) for a `cards_bk` refcount
/// field. Each generated function operates on the `flags_bk` u32:
///
/// - `<name>(flags_bk)` — read the field's current value.
/// - `write_<name>(flags_bk, count)` — replace the field's value
///   (count is masked to the field width).
/// - `increment_<name>(flags_bk)` — saturating-add one to the field,
///   capped at the field's max value.
/// - `decrement_<name>(flags_bk)` — saturating-sub one from the field,
///   floor at zero.
///
/// Field shape (mask / shift / max) is pulled from `bk_flags()`
/// keyed on `$mask` / `$shift` / `$max` (member names on `BkFlags`).
/// The acquire / release / propagate-forward triplet that touches
/// the cards table is generated separately via `decl_count_ctx!`.
macro_rules! decl_count_pure {
    (
        $field_name:ident,
        $mask_field:ident,
        $shift_field:ident,
        $max_field:ident,
        $read:ident,
        $write:ident,
        $increment:ident,
        $decrement:ident $(,)?
    ) => {
        #[doc = concat!("Read the `", stringify!($field_name), "` field out of a `flags_bk` u32.")]
        pub fn $read(flags_bk: u32) -> u32 {
            let b = bk_flags();
            (flags_bk & b.$mask_field) >> b.$shift_field
        }

        #[doc = concat!("Replace the `", stringify!($field_name), "` field on a `flags_bk` u32 with `count` (clamped to the field width).")]
        fn $write(flags_bk: u32, count: u32) -> u32 {
            let b = bk_flags();
            (flags_bk & !b.$mask_field)
                | ((count & b.$max_field) << b.$shift_field)
        }

        #[doc = concat!("Pure flag transform: bump `", stringify!($field_name), "` by 1 (saturating at the field's max). Used by callers with an open `update_with(_at)` closure on `flags_bk`; pair with the corresponding `propagate_*_forward` to extend the delta into future rows, or prefer the higher-level acquire helper.")]
        pub fn $increment(flags_bk: u32) -> u32 {
            let max = bk_flags().$max_field;
            let next = $read(flags_bk).saturating_add(1).min(max);
            $write(flags_bk, next)
        }

        #[doc = concat!("Pure flag transform: subtract 1 from `", stringify!($field_name), "` (saturating at 0). Mirror of `", stringify!($increment), "`.")]
        pub fn $decrement(flags_bk: u32) -> u32 {
            let next = $read(flags_bk).saturating_sub(1);
            $write(flags_bk, next)
        }
    };
}

/// Define the acquire / release / propagate-forward triplet for a
/// `cards_bk` refcount field. Wraps the pure transforms from
/// `decl_count_pure!` with the cards-table mutation and forward-prop
/// walk. Each generated `acquire_<name>` increments the count at
/// `time_ms` and walks every future-stamped row, applying +1 with
/// delta arithmetic; `release_<name>` mirrors with -1.
///
/// The forward-prop helper bypasses `write_at` (direct
/// `delete` / `insert`) so we don't recursively fire hooks or
/// re-enqueue schedule-deletes for rows that were already scheduled
/// at their original write. `valid_at` PKs are preserved across the
/// delete/insert pair so any existing `schedule_delete_cards` row
/// keyed on it still targets the correct (replacement) row.
macro_rules! decl_count_ctx {
    (
        $increment:ident,
        $decrement:ident,
        $acquire:ident,
        $release:ident,
        $propagate:ident $(,)?
    ) => {
        #[doc = concat!("Acquire one reference on a card at `time_ms`. Bumps the count on the row current at that time AND walks every future-stamped row, incrementing there too — so a future-stamped release row written by an earlier action (with count baked in) correctly reflects 'but someone else is still holding' once this acquire lands.")]
        pub fn $acquire(ctx: &ReducerContext, card_id: u32, time_ms: u64) {
            update_with_at(ctx, card_id, time_ms, |c| {
                c.flags_bk = $increment(c.flags_bk);
            });
            $propagate(ctx, card_id, time_ms, true);
        }

        #[doc = concat!("Release one reference on a card at `time_ms`. Decrements the count at `time_ms` and on every future-stamped row of the same card.")]
        pub fn $release(ctx: &ReducerContext, card_id: u32, time_ms: u64) {
            update_with_at(ctx, card_id, time_ms, |c| {
                c.flags_bk = $decrement(c.flags_bk);
            });
            $propagate(ctx, card_id, time_ms, false);
        }

        #[doc = concat!("Apply ±1 to the field on every row of this card with `valid_at_time > time_ms`. Bypasses `write_at` (direct `delete` / `insert`) so we don't re-fire hooks. `valid_at` PKs are preserved across delete/insert pairs.")]
        pub fn $propagate(
            ctx: &ReducerContext,
            card_id: u32,
            time_ms: u64,
            increment: bool,
        ) {
            let future: Vec<u64> = ctx
                .db
                .cards()
                .card_id()
                .filter(card_id)
                .filter(|c| valid_at_time(c.valid_at) > time_ms)
                .map(|c| c.valid_at)
                .collect();
            for v in future {
                let Some(row) = ctx.db.cards().valid_at().find(v) else {
                    continue;
                };
                let new_bk = if increment {
                    $increment(row.flags_bk)
                } else {
                    $decrement(row.flags_bk)
                };
                ctx.db.cards().valid_at().delete(v);
                let mut updated = row;
                updated.flags_bk = new_bk;
                ctx.db.cards().insert(updated);
            }
        }
    };
}

decl_count_pure!(
    position_hold_count,
    position_hold_count_mask,
    position_hold_count_shift,
    position_hold_count_max,
    position_hold_count,
    write_position_hold_count,
    increment_position_hold_count,
    decrement_position_hold_count,
);

decl_count_pure!(
    slot_share_count,
    slot_share_count_mask,
    slot_share_count_shift,
    slot_share_count_max,
    slot_share_count,
    write_slot_share_count,
    increment_slot_share_count,
    decrement_slot_share_count,
);

decl_count_pure!(
    drop_hold_count,
    drop_hold_count_mask,
    drop_hold_count_shift,
    drop_hold_count_max,
    drop_hold_count,
    write_drop_hold_count,
    increment_drop_hold_count,
    decrement_drop_hold_count,
);

decl_count_pure!(
    slot_hold_count,
    slot_hold_count_mask,
    slot_hold_count_shift,
    slot_hold_count_max,
    slot_hold_count,
    write_slot_hold_count,
    increment_slot_hold_count,
    decrement_slot_hold_count,
);

decl_count_pure!(
    touch_count,
    touch_count_mask,
    touch_count_shift,
    touch_count_max,
    touch_count,
    write_touch_count,
    increment_touch_count,
    decrement_touch_count,
);

decl_count_pure!(
    server_count,
    server_count_mask,
    server_count_shift,
    server_count_max,
    server_count,
    write_server_count,
    increment_server_count,
    decrement_server_count,
);

/// State-flag bit mask carried by `packed_definition`'s
/// `CardDefinition.flags` (def-driven inheritance — today this
/// surfaces only the `magnetic` lifecycle-pending bit), or 0 when the
/// definition isn't registered. Called from [`create`] / [`create_at`]
/// so any card spawned with a definition inherits its state-flag set
/// without per-call-site bookkeeping. Def-driven `flags_bk` doesn't
/// exist yet — no bookkeeping flag has an authoring-time meaning.
fn definition_state_flag_mask(packed: u16) -> u32 {
    decode_definition(packed)
        .ok()
        .flatten()
        .map_or(0, |def| def.flags)
}

#[table(accessor = cards, public)]
pub struct Card {
    #[primary_key]
    pub valid_at: u64,
    #[index(btree)]
    pub card_id: u32,
    pub surface: u8,
    #[index(btree)]
    pub macro_zone: u64,
    pub micro_zone: u8,
    /// Index motivator: state-3 (`StackedState::Deferred`) cards
    /// carry their host's `card_id` in `micro_location`. When the
    /// host's `(surface, macro_zone)` changes, every state-3 row
    /// pointing at it must follow — `state_3_followers(host_id)`
    /// scans this index, dedupes by card_id, filters to current
    /// state-3 rows. Also covers the more general case (state-1
    /// Slot points at parent here too) but the only consumer today
    /// is the state-3 cascade in [`write_at`].
    #[index(btree)]
    pub micro_location: u32,
    #[index(btree)]
    pub owner_id: u32,
    pub packed_definition: u16,
    /// State flags — what is true about the card. Propagated forward
    /// by [`propagate_flag_diff_forward`] from [`write_at`] on every
    /// insert. Bit layout lives in `content/cards/flags.json`'s
    /// `cards_state` namespace and is surfaced server-side via
    /// [`crate::flags::state_flags`] (cached at first access).
    pub flags_state: u32,
    /// Bookkeeping flags — server-managed dirty / preserve markers
    /// plus refcount fields (`position_hold_count`, `slot_share_count`).
    /// Never bit-diff propagated; refcount fields have dedicated
    /// delta-arithmetic propagators
    /// ([`propagate_position_hold_forward`] /
    /// [`propagate_slot_share_forward`]) that handle overlapping
    /// holders correctly. Bit layout lives in `cards_bk` and is
    /// surfaced server-side via [`crate::flags::bk_flags`].
    pub flags_bk: u32,
}

/// Wall-clock now in unix milliseconds (u64). The codebase's time
/// unit throughout: `valid_at` rows pack u48 ms, recipe-duration
/// arithmetic operates on ms, etc. Convert from the microsecond
/// timestamp the SpacetimeDB scheduler provides.
pub fn now_ms(ctx: &ReducerContext) -> u64 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64
}

/// Row that's *current at wall-clock now* — max `valid_at_time` among
/// rows with `valid_at_time ≤ now`. Future-stamped rows (recipe
/// completions / movement queues scheduled ahead of now) are
/// intentionally excluded: callers asking for "the latest" mean the
/// state the client would currently be promoting to, not the deepest
/// future row in the history.
///
/// Mirrors the discipline already used by `zones::set_tile_at` and
/// `souls::apply_slot_delta` (read at `≤ time_secs` rather than
/// unbounded max), so card state observed by validation / owner
/// lookups / hook diffs stays consistent across all history-style
/// tables.
pub fn latest(ctx: &ReducerContext, card_id: u32) -> Option<Card> {
    prior_at(ctx, card_id, now_ms(ctx))
}

/// Client-server time-drift tolerance.
///
/// Single-source-of-truth for the time-discipline contract between
/// client and server:
///
///  - **Client buffer:** the client runs its `serverNowMs()` estimate
///    `TIME_DRIFT_BUFFER_MS` behind the captured server timestamp, so
///    `client_time_ms` submitted with every reducer is ≈ server_time -
///    TIME_DRIFT_BUFFER_MS in steady state.
///  - **Server back-grace:** the server accepts `client_time_ms` up to
///    `TIME_DRIFT_BUFFER_MS + MAX_RTT_MS` behind its own clock. The
///    buffer term absorbs the client's normal lag; the RTT term
///    absorbs the round-trip network latency the submission travels
///    through (since `ctx.timestamp` includes the inbound `δ_out`
///    that the client couldn't yet have observed). Anything further
///    back is rejected as `time_drift:client_behind`.
///  - **Server forward-grace:** the server accepts `client_time_ms` up
///    to `TIME_DRIFT_BUFFER_MS` ahead of its own clock. Beyond that the
///    server rejects as `time_drift:client_ahead`. Inside the window
///    the server uses `min(client, server)` for game logic, so any
///    forward overshoot just degrades to using server-time directly.
///
/// 2000ms covers ~3% server-clock drag (observed in WSL2 / Docker
/// throttling) over recipes up to ~67 seconds. If longer-duration
/// content surfaces or the drift baseline worsens, this constant is
/// the single knob.
pub const TIME_DRIFT_BUFFER_MS: u64 = 2_000;

/// Maximum round-trip network latency the time-discipline contract
/// tolerates. Used by [`effective_now_ms`] as additional back-grace
/// on top of `TIME_DRIFT_BUFFER_MS`.
///
/// Why this exists: in steady state, a client submission travels to
/// the server with `δ_out` of latency, so `ctx.timestamp` reads
/// `client_time_ms + TIME_DRIFT_BUFFER_MS + RTT`. Without this term,
/// any client whose round-trip exceeds the buffer magnitude (e.g.,
/// remote servers, slow Wi-Fi, mobile) would trip the back-grace
/// check even when its clock is perfectly aligned with the server's.
///
/// 3000ms covers high-latency dev setups (remote SpacetimeDB across a
/// public link) and intermittent jitter on otherwise-OK connections.
/// The cheat budget under this constant is `MAX_RTT_MS`: a malicious
/// client can back-date submissions by up to that amount per call to
/// shave duration waits. Bounded by chained dependencies as elsewhere.
pub const MAX_RTT_MS: u64 = 3_000;

/// Static backward-grace window in `effective_now_ms`. The server
/// accepts `client_time_ms` up to this many ms behind its own clock,
/// rejecting anything older as `time_drift:client_behind_by`.
///
/// Sized at 2× the client's maximum self-imposed `clientDelay` (= 5s
/// on the client), so the server has comfortable headroom even when
/// the client is operating at its max-clamp. No per-player budget is
/// stored or communicated — the client adapts within `[1500, 5000]`
/// independently, and as long as that ceiling holds the server never
/// rejects a legitimate submission for being too far behind. Keep
/// `CLIENT_DELAY_MAX_MS` on the client ≤ `N/2` to maintain the 2×
/// safety margin.
pub const BACKWARD_GRACE_MS: u64 = 10_000;

/// Resolve the time to use for game-logic calculations in a reducer.
///
/// Policy:
///  - Reject if `client_time_ms` is more than `BACKWARD_GRACE_MS`
///    behind the server's `ctx.timestamp`. Static cap; the client
///    adapts its own `clientDelay` independently within `[1500, 5000]`
///    and the server just needs enough margin to cover the client's
///    max self-imposed delay (sized at 2×).
///  - Reject if `client_time_ms` is more than `TIME_DRIFT_BUFFER_MS`
///    ahead of the server's `ctx.timestamp`. (Cheat / desync: client
///    claims the future to pull rows whose `valid_at` hasn't elapsed.
///    Forward grace stays small — it's anti-cheat, not lag absorption.)
///  - Otherwise return `min(client_time_ms, server_time_ms)`.
///
/// **Why `min`:** when the client is correctly behind the server (the
/// normal case under steady-state buffer), `min` picks the client's
/// value, so server stamps new rows at `client_time + duration*1000`.
/// The client's `serverNowMs()` then hits those `valid_at`s on
/// schedule. When the client is anomalously ahead (within the forward
/// grace), `min` picks server, falling back to current-strict behavior.
///
/// Errors use the `time_drift:` prefix so the client's `ActionManager`
/// can parse the rejection and schedule a retry once the gap closes.
/// Format: `time_drift:client_behind_by=<N>` or
/// `time_drift:client_ahead_by=<N>` where N is the millisecond gap.
pub fn effective_now_ms(ctx: &ReducerContext, client_time_ms: u64) -> Result<u64, String> {
    let server = now_ms(ctx);
    // Client too far in the past → reject. `saturating_sub` keeps the
    // math safe if `client_time_ms` is somehow huge (overflow guard).
    let behind = server.saturating_sub(client_time_ms);
    if behind > BACKWARD_GRACE_MS {
        return Err(format!(
            "time_drift:client_behind_by={behind} (server={server}, client={client_time_ms})"
        ));
    }
    // Client too far in the future → reject. Forward grace is static
    // (anti-cheat, not lag absorption).
    let ahead = client_time_ms.saturating_sub(server);
    if ahead > TIME_DRIFT_BUFFER_MS {
        return Err(format!(
            "time_drift:client_ahead_by={ahead} (server={server}, client={client_time_ms})"
        ));
    }
    Ok(client_time_ms.min(server))
}

/// Row that's current at `time_ms` — max `valid_at_time` among
/// rows with `valid_at_time ≤ time_ms`. Used internally by
/// `write_at` / `update_with` / `update_with_at` for diffing against
/// the time we're writing at (which may be future-stamped), and
/// externally by `on_create::trigger` to inspect a card whose only
/// row is future-stamped from its creating action (the
/// `cards::create_at(...completion_ms)` path in
/// `action_completion::apply`). Same query as [`latest`], with the
/// upper bound parameterised instead of hard-pinned to `now`.
pub fn prior_at(ctx: &ReducerContext, card_id: u32, time_ms: u64) -> Option<Card> {
    ctx.db
        .cards()
        .card_id()
        .filter(card_id)
        .filter(|c| valid_at_time(c.valid_at) <= time_ms)
        .max_by_key(|c| valid_at_time(c.valid_at))
}

/// Delete every row for `card_id` whose `valid_at_time` matches
/// exactly `time_ms`. Used by `write_at` to purge a stale row at the
/// same time before re-stamping, so in-reducer accumulation patterns
/// (e.g., `souls::apply_slot_delta` firing N times at one `now_ms`)
/// don't leave behind duplicates with stale seq values. There's
/// usually 0 or 1 match; if multiple rows somehow share a time (which
/// shouldn't happen under the global sequence allocator), every match
/// is deleted to preserve "no leftover at this time" semantics.
pub fn delete_at(ctx: &ReducerContext, card_id: u32, time_ms: u64) {
    let pks: Vec<u64> = ctx
        .db
        .cards()
        .card_id()
        .filter(card_id)
        .filter(|c| valid_at_time(c.valid_at) == time_ms)
        .map(|c| c.valid_at)
        .collect();
    for pk in pks {
        ctx.db.cards().valid_at().delete(pk);
    }
}

// Stamp valid_at = (card_id, now) and write. If a row already exists at that
// exact key (two writes in the same second), the existing one is replaced —
// "always accept the most recent write". Also enqueues a one-shot delete
// schedule that will sweep older rows for this card_id once the scheduler
// fires.
fn write(ctx: &ReducerContext, card: Card) -> Card {
    write_at(ctx, card, now_ms(ctx))
}

// Like `write`, but stamps valid_at with a caller-supplied second-precision
// timestamp instead of `now`. Used by the action-completion path to apply
// product generation / reagent consumption / flag release at the action's
// scheduled completion time rather than at "scheduler tick" time.
//
// After the insert, the souls module's `on_card_write` hook fires.
// That hook is responsible for keeping the `Soul` table in sync with
// the cards table — soul-card positional mirroring (when this write
// is the soul card itself) and stat-counter diffing (when this write
// changes a tracked faculty card). Capturing `prev_latest` BEFORE
// the find/delete/insert is essential: it's the row the diff
// compares against, and it's exactly the row we may be about to
// replace at the same PK. Doing this here keeps every code path that
// writes cards (action_completion, on_create, magnetic, movement,
// utilities, world_gen) automatically participating in soul
// tracking — there's only one write entry point.
fn write_at(ctx: &ReducerContext, mut card: Card, time_ms: u64) -> Card {
    // Prior state for the auto-diff / souls hook / forward-prop is
    // the row that was current *at the time we're writing*, not the
    // deepest future row. Reading unbounded max would pull in a
    // future-stamped row's data and miscompute every downstream
    // signal (dirty flags, stat contributions, flag set/clear deltas).
    // Read this BEFORE the same-time delete below — that delete may
    // remove the very row this returns, but we've already snapshotted
    // its value into `prev_latest`.
    let prev_latest = prior_at(ctx, card.card_id, time_ms);
    // "Last write at this (card_id, time_ms) wins." Under the old
    // packing the PK was `(card_id << 32) | time_secs`, so two writes
    // for the same card at the same second produced the same PK and
    // the `find/delete/insert` below overwrote the previous row. The
    // new packing makes the seq portion unique per call, so without
    // this explicit purge we'd accumulate one stale row per same-time
    // write — breaking in-reducer accumulation patterns like
    // `souls::apply_slot_delta` firing N times at one `now_ms` (which
    // is exactly what `bootstrap`'s 3-corpus add does, and which
    // showed up as "soul.stats=1 instead of 3" before this purge).
    delete_at(ctx, card.card_id, time_ms);
    // PK = (time_ms << 16) | seq. The sequence is a global u16 from
    // `sequence::next_sequence`, fresh per write — guarantees PK
    // uniqueness across DIFFERENT cards writing at the same ms.
    // See `packed::pack_valid_at` and `sequence.rs`.
    card.valid_at = pack_valid_at(time_ms, sequence::next_sequence(ctx));

    // Auto-set `position_dirty` / `data_dirty` (both in `flags_bk`)
    // by diffing the row we're about to insert against the prior row.
    // Strips any caller-set value for those bits (they're
    // server-managed) but preserves the caller-set `position_preserve`
    // / `data_preserve` intent flags. With no prior row (fresh
    // card_id), treat everything as dirty so the very first row
    // carries the markers any future forward-prop will key off.
    let bk = bk_flags();
    card.flags_bk &= !(bk.position_dirty | bk.data_dirty);
    let (auto_pos, auto_data) = match prev_latest.as_ref() {
        Some(prev) => {
            let pos_changed = card.surface != prev.surface
                || card.macro_zone != prev.macro_zone
                || card.micro_zone != prev.micro_zone
                || card.micro_location != prev.micro_location;
            // Data diff: `flags_state` is the data field directly —
            // bookkeeping bits live in `flags_bk` (this field's
            // own job) so they can't pollute the diff.
            let data_changed = card.owner_id != prev.owner_id
                || card.packed_definition != prev.packed_definition
                || card.flags_state != prev.flags_state;
            (pos_changed, data_changed)
        }
        None => (true, true),
    };
    if auto_pos {
        card.flags_bk |= bk.position_dirty;
    }
    if auto_data {
        card.flags_bk |= bk.data_dirty;
    }

    if ctx.db.cards().valid_at().find(card.valid_at).is_some() {
        ctx.db.cards().valid_at().delete(card.valid_at);
    }
    let inserted = ctx.db.cards().insert(card);
    // No per-write delete schedule — reaping is handled by the
    // periodic GC sweep ([`crate::gc`]) which applies retention
    // rules over the whole cards table.
    crate::souls::on_card_write(ctx, prev_latest.as_ref(), &inserted, time_ms);
    if let Some(prev) = prev_latest.as_ref() {
        propagate_flag_diff_forward(ctx, &inserted, prev.flags_state, time_ms);
    }
    cascade_to_state_3_followers(ctx, prev_latest.as_ref(), &inserted, time_ms);
    on_card_write_lifecycle(ctx, prev_latest.as_ref(), &inserted, time_ms);
    inserted
}

/// Every card currently in `StackedState::Deferred` whose
/// `micro_location == host_id`. Walks the `micro_location` btree
/// index, dedupes by `card_id`, looks up each card's latest row via
/// [`prior_at`], filters down to those whose latest state is
/// Deferred. The dedup-then-latest pattern mirrors
/// [`walk_branch_top`] in `place.rs`: the index returns every
/// historical row pointing at the host (including stale ones), but
/// we only care about the current shape per card.
///
/// O(matches) on the index, bounded in practice by the per-host
/// deferred follower count (typically 0-2 for normal recipe play).
fn state_3_followers(ctx: &ReducerContext, host_id: u32, time_ms: u64) -> Vec<Card> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut out: Vec<Card> = Vec::new();
    for row in ctx.db.cards().micro_location().filter(host_id) {
        if !seen.insert(row.card_id) {
            continue;
        }
        let Some(latest) = prior_at(ctx, row.card_id, time_ms) else {
            continue;
        };
        if latest.micro_location != host_id {
            continue; // host pointer was stale; the latest row points elsewhere now.
        }
        let (_, _, state) = unpack_micro_zone(latest.micro_zone);
        if !matches!(state, StackedState::Deferred) {
            continue;
        }
        out.push(latest);
    }
    out
}

/// Cascade host-side `(surface, macro_zone)` / dead-flag changes
/// down to every state-3 follower. Runs from [`write_at`] after the
/// new row is inserted and the flag-diff propagator has finished,
/// so the follower writes see the host's settled state.
///
/// Three trigger cases:
///
/// 1. **Host moved between `(surface, macro_zone)` pairs.** Rewrite
///    every follower's surface + macro_zone to match. Without this,
///    a follower placed in zone A would stay in A while the host
///    travels to zone B — players subscribed to B alone can't see
///    the follower; players subscribed to A see a follower pointing
///    at a host they don't have. The cascade keeps the deferred
///    chain visible together.
///
/// 2. **Host became dead.** Clear each follower's `micro_location`
///    to 0 — the host's gone, the resolution anchor is invalid.
///    Client cascade then falls through to the fallback (q, r) tier
///    on the next mirror pass. Done at dead-flag-set time (not at
///    actual delete) so the resolution happens before the GC sweep
///    races the renderer.
///
/// 3. **No-op case.** Host's position and dead-flag both unchanged
///    from the prior row → followers untouched (the most common
///    path; a flags-only or stock-only update on a stable host).
fn cascade_to_state_3_followers(
    ctx: &ReducerContext,
    prev: Option<&Card>,
    new: &Card,
    time_ms: u64,
) {
    let s = crate::flags::state_flags();
    let position_changed = match prev {
        Some(p) => p.surface != new.surface || p.macro_zone != new.macro_zone,
        None => false, // first row for this card — no followers can be anchored yet.
    };
    let became_dead = match prev {
        Some(p) => (p.flags_state & s.dead) == 0 && (new.flags_state & s.dead) != 0,
        None => (new.flags_state & s.dead) != 0,
    };
    if !position_changed && !became_dead {
        return;
    }
    let followers = state_3_followers(ctx, new.card_id, time_ms);
    if followers.is_empty() {
        return;
    }
    for follower in followers {
        if became_dead {
            // Host's gone — clear the anchor so the client cascade
            // falls through to the fallback (q, r) tier.
            update_with_at(ctx, follower.card_id, time_ms, |c| {
                c.micro_location = 0;
            });
        } else if position_changed {
            // Host moved zones — pull the follower along.
            update_with_at(ctx, follower.card_id, time_ms, |c| {
                c.surface = new.surface;
                c.macro_zone = new.macro_zone;
            });
        }
    }
}

/// Lifecycle-pending bookkeeping hook fired from `write_at` after the
/// row is inserted and the souls / flag-prop hooks have run.
///
/// Two transitions to handle, both gated on prev-vs-new flag state:
///
/// - **Install (first row for a lifecycle card).** `prev_latest` is
///   `None` AND the new row carries `FLAG_LIFECYCLE_PENDING`. Read
///   `def.lifecycle_duration_ms` to derive `expires_at`, walk
///   `owning_player` to find the responsible player, insert the
///   detail row, re-sync the owner's `PlayerProfile` summary.
///
/// - **Dead transition.** `prev.flags & FLAG_DEAD == 0` AND
///   `new.flags & FLAG_DEAD != 0`. The card is being consumed —
///   remove any lifecycle_pending row (no-op if not lifecycle) and
///   re-sync the owner's summary.
///
/// Out-of-band transitions (def-change without death, lifecycle
/// flag cleared without death) are NOT covered here — current
/// content design has recipes consume their lifecycle root, so
/// death is the canonical cleanup trigger. If a recipe-of-the-
/// future transforms without killing, that path needs its own hook.
fn on_card_write_lifecycle(
    ctx: &ReducerContext,
    prev: Option<&Card>,
    new_row: &Card,
    _time_ms: u64,
) {
    let s = state_flags();
    let prev_dead = prev.map_or(false, |p| p.flags_state & s.dead != 0);
    let new_dead = new_row.flags_state & s.dead != 0;
    let became_dead = !prev_dead && new_dead;

    // Install: first-ever row carrying the lifecycle flag (the
    // `magnetic` bit in `flags_state`, set via def-flag inheritance
    // by `definition_state_flag_mask`).
    let is_first_row = prev.is_none();
    let is_lifecycle = new_row.flags_state & s.magnetic != 0;
    if is_first_row && is_lifecycle && !new_dead {
        // Pull `lifecycle_duration_ms` off the def. Decode failures
        // are silently skipped here (content-authoring errors should
        // surface via `validate_lifecycle_recipes` at test time
        // rather than crash the write path). A lifecycle card whose
        // def has no duration is malformed but the cards table
        // itself remains consistent.
        if let Some(duration_ms) = decode_definition(new_row.packed_definition)
            .ok()
            .flatten()
            .and_then(|def| def.lifecycle_duration_ms)
        {
            let install_ms = valid_at_time(new_row.valid_at);
            let expires_at_ms = install_ms.saturating_add(duration_ms as u64);
            // owning_player walks up the owner chain to find a row
            // carrying FLAG_OWNED_BY_PLAYER. World-owned cards return
            // WORLD_PLAYER_ID (= 0); the detail row is still inserted
            // but the block-check filters player_id=0 rows out (no
            // one to block).
            let player_id = owning_player(ctx, new_row.card_id).unwrap_or(WORLD_PLAYER_ID);
            crate::lifecycle_pending::install(ctx, new_row.card_id, expires_at_ms, player_id);
            crate::players::resync_lifecycle_summary(ctx, player_id);
        }
    }

    // Dead transition: clean up regardless of whether the card was
    // lifecycle. The remove is a no-op for non-lifecycle cards.
    if became_dead {
        // Find the player_id BEFORE removing the row (the row's
        // player_id field is the authoritative attribution; the
        // current owner chain may have shifted).
        let player_id = ctx
            .db
            .lifecycle_pending()
            .card_id()
            .find(new_row.card_id)
            .map(|r| r.player_id);
        crate::lifecycle_pending::remove(ctx, new_row.card_id);
        if let Some(pid) = player_id {
            crate::players::resync_lifecycle_summary(ctx, pid);
        }
    }
}

/// Forward-propagate the flag delta between `prev_flags` (the row our
/// `write_at` just replaced) and `new_card.flags` (the row we just
/// wrote) into every existing future-stamped row for the same card,
/// with stop-on-deliberate-change discipline per bit.
///
/// The motivating case: a fleeting card has a future-stamped dead row
/// (from `on_create.fleeting`'s consume) scheduled at `T_expire`. A
/// player drops it into another action at `T_propose < T_expire`,
/// which sets `slot_hold` on the card at `T_propose`. Without
/// forward-propagation, the `T_expire` row still has `slot_hold = 0`,
/// so client-side death-animation gates that check `slot_hold == 0`
/// fire prematurely while the action is mid-flight.
///
/// Per-bit logic, mirroring the zones-forward-propagation pattern but
/// generalised across all 32 flag bits:
///
/// - `set_bits = new & !prev` — bits we just turned on.
/// - `clear_bits = prev & !new` — bits we just turned off.
/// - For each future row in ascending `valid_at_time`, narrow the
///   active set/clear masks down to bits whose value in that row
///   *still matches* the prior's value (set bits where the row has 0;
///   clear bits where the row has 1). Bits that diverged were
///   deliberately changed by some other write — drop them from the
///   active mask and don't second-guess that decision. Apply the
///   surviving mask to the row.
///
/// Bypasses `write_at` for the future-row updates (direct
/// `delete` / `insert`) so we don't recursively fire hooks or
/// re-enqueue schedule-deletes for rows that were already scheduled
/// at their original write. The `valid_at` PK is preserved across the
/// delete/insert pair, so any existing `schedule_delete_cards` row
/// keyed on it still targets the correct (replacement) row.
fn propagate_flag_diff_forward(
    ctx: &ReducerContext,
    new_card: &Card,
    prev_flags_state: u32,
    time_ms: u64,
) {
    // Operates solely on `flags_state`. Bookkeeping bits live in
    // `flags_bk` (their own column) so they can't pollute the diff —
    // no exclusion mask needed.
    let set_bits = new_card.flags_state & !prev_flags_state;
    let clear_bits = prev_flags_state & !new_card.flags_state;
    if set_bits == 0 && clear_bits == 0 {
        return;
    }

    let mut future: Vec<u64> = ctx
        .db
        .cards()
        .card_id()
        .filter(new_card.card_id)
        .filter(|c| valid_at_time(c.valid_at) > time_ms)
        .map(|c| c.valid_at)
        .collect();
    future.sort_unstable_by_key(|v| valid_at_time(*v));

    let mut active_set = set_bits;
    let mut active_clear = clear_bits;
    let data_preserve = bk_flags().data_preserve;

    for v in future {
        if active_set == 0 && active_clear == 0 {
            break;
        }
        let Some(row) = ctx.db.cards().valid_at().find(v) else {
            continue;
        };
        let row_state = row.flags_state;

        // `data_preserve` (in flags_bk) rows opt out of forward-prop
        // entirely — the caller deliberately stamped a state at this
        // point in time and the per-bit stop-on-change rule can't
        // distinguish "deliberate-same-as-prior" from "inherited," so
        // we honor the explicit intent. Skip this row but keep
        // walking — a preserve marker means "don't touch THIS row,"
        // not "stop propagation entirely."
        if row.flags_bk & data_preserve != 0 {
            continue;
        }

        // Narrow per-bit:
        //  - For set_bits: keep bits where row has 0 (matches prev's
        //    0, so the row inherited from before our write). Drop
        //    bits where row has 1 (someone deliberately set it).
        //  - For clear_bits: keep bits where row has 1 (matches
        //    prev's 1, so inherited). Drop bits where row has 0
        //    (someone deliberately cleared it).
        active_set &= !row_state;
        active_clear &= row_state;
        if active_set == 0 && active_clear == 0 {
            break;
        }

        let new_state = (row_state & !active_clear) | active_set;
        ctx.db.cards().valid_at().delete(v);
        let mut updated = row;
        updated.flags_state = new_state;
        ctx.db.cards().insert(updated);
    }
}

/// Walk future-stamped rows of `card_id` and reconcile them against
/// a new position-change happening at `after`. Three behaviours per
/// row, gated by the dirty / preserve markers:
///
/// - **`position_preserve` set** → STOP. This row's position is
///   author-pinned (recipe chain stitch, magnetic anchor, etc.) and
///   so is everything past it. Don't touch this or any later row.
/// - **`position_dirty` set, `data_dirty` clear** → DELETE. Pure
///   position row, almost certainly a movement step from an earlier
///   queue that this new write supersedes. Drop it.
/// - **Otherwise** (row carries data, or is empty/neither) → UPDATE
///   the four position fields to `new_surface` / `new_macro_zone` /
///   `new_micro_zone` / `new_micro_location`, leave flags / owner_id
///   / packed_definition alone. The row's data intent survives; its
///   position is now consistent with our new write.
///
/// Bypasses `write_at` for row updates — the rewritten rows would
/// otherwise re-fire the souls hook and re-enqueue schedule-deletes.
/// `valid_at` PKs are preserved across delete/insert pairs so any
/// existing `schedule_delete_cards` row still targets the
/// replacement row correctly.
///
/// Movement-interruption callers (`move_soul`, future teleport /
/// push reducers) invoke this with the soul card's new position
/// just BEFORE writing their own row at `after`. Order matters:
/// scrubbing before the write means the new write's own flag
/// forward-prop has fewer (and consistent) future rows to walk.
pub fn scrub_or_repath_position_forward(
    ctx: &ReducerContext,
    card_id: u32,
    after_ms: u64,
    new_surface: u8,
    new_macro_zone: u64,
    new_micro_zone: u8,
    new_micro_location: u32,
) {
    let mut future: Vec<u64> = ctx
        .db
        .cards()
        .card_id()
        .filter(card_id)
        .filter(|c| valid_at_time(c.valid_at) > after_ms)
        .map(|c| c.valid_at)
        .collect();
    future.sort_unstable_by_key(|v| valid_at_time(*v));

    let b = bk_flags();
    for v in future {
        let Some(row) = ctx.db.cards().valid_at().find(v) else {
            continue;
        };
        let row_bk = row.flags_bk;

        if row_bk & b.position_preserve != 0 {
            // Author-pinned position. This row and everything past
            // it is off-limits — chain stitch / magnetic pull
            // shouldn't be yanked mid-action.
            break;
        }

        let pos_only =
            (row_bk & b.position_dirty != 0) && (row_bk & b.data_dirty == 0);
        if pos_only {
            ctx.db.cards().valid_at().delete(v);
            continue;
        }

        // Mixed or data-only row — re-home its position fields to
        // the new destination. Leave the data fields (flags_state /
        // owner / packed_def) alone so a future flag-change row still
        // applies its intended change at the correct moment.
        let pos_changed = row.surface != new_surface
            || row.macro_zone != new_macro_zone
            || row.micro_zone != new_micro_zone
            || row.micro_location != new_micro_location;
        ctx.db.cards().valid_at().delete(v);
        let mut updated = row;
        updated.surface = new_surface;
        updated.macro_zone = new_macro_zone;
        updated.micro_zone = new_micro_zone;
        updated.micro_location = new_micro_location;
        // Only mark `position_dirty` if the re-home actually moved
        // the row. A no-op re-home (destination equals the row's
        // existing position — common for anchor rows on a quick
        // interrupt where soul.latest still equals the old anchor's
        // position) shouldn't gain `position_dirty`, otherwise the
        // client's tween resolver would mistake a same-position
        // anchor for a tween target and stutter at the start of
        // the new path.
        if pos_changed {
            updated.flags_bk |= b.position_dirty;
        }
        ctx.db.cards().insert(updated);
    }
}

// Insert a brand-new card. valid_at is computed; pass 0 will be overwritten.
//
// `flags_state` is OR'd with `definition_state_flag_mask(packed_definition)`
// so every card spawned with a definition inherits its def-driven state
// flags (today: the `magnetic` bit for lifecycle-pending defs). `flags_bk`
// is taken from the caller verbatim — no def-driven bookkeeping flags
// exist. Callers spawning a soul pass `state_flags().is_owned_by_player`
// plus the portrait field via `with_portrait`; most other callsites
// pass `(0, 0)` and let the def-flag inheritance do the work.
#[allow(clippy::too_many_arguments)]
pub fn create(
    ctx: &ReducerContext,
    card_id: u32,
    surface: u8,
    macro_zone: u64,
    micro_zone: u8,
    micro_location: u32,
    owner_id: u32,
    packed_definition: u16,
    flags_state: u32,
    flags_bk: u32,
) -> Card {
    write(
        ctx,
        Card {
            valid_at: 0,
            card_id,
            surface,
            macro_zone,
            micro_zone,
            micro_location,
            owner_id,
            packed_definition,
            flags_state: flags_state | definition_state_flag_mask(packed_definition),
            flags_bk,
        },
    )
}

// Pick up the row that's current at wall-clock `now`, mutate it via
// `f`, write it back. Returns `None` if no prior row exists. Reads
// the *prior* row (max `valid_at_time ≤ now`) rather than unbounded
// max so the mutation operates on the state the client would
// currently see, not a future-stamped destination from some queued
// path / recipe completion.
pub fn update_with<F>(ctx: &ReducerContext, card_id: u32, f: F) -> Option<Card>
where
    F: FnOnce(&mut Card),
{
    let now = now_ms(ctx);
    let mut c = prior_at(ctx, card_id, now)?;
    f(&mut c);
    Some(write_at(ctx, c, now))
}

// Like `update_with`, but stamps the resulting row at a specific
// `time_ms` rather than `now`. Used by the action-completion /
// movement / on_create paths that future-stamp completions. Reads
// the prior row at `≤ time_ms` so writes interleaved in time
// don't pick up state from a deeper-future row (same fix the zones /
// souls write helpers already apply).
pub fn update_with_at<F>(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    f: F,
) -> Option<Card>
where
    F: FnOnce(&mut Card),
{
    let mut c = prior_at(ctx, card_id, time_ms)?;
    f(&mut c);
    Some(write_at(ctx, c, time_ms))
}

// Generated acquire / release / propagate-forward triplets for each
// `cards_bk` refcount field. See `decl_count_ctx!` and the unified-
// hold-counts rework doc (`docs/UNIFIED_HOLD_COUNTS.md`).
decl_count_ctx!(
    increment_position_hold_count,
    decrement_position_hold_count,
    acquire_position_hold,
    release_position_hold,
    propagate_position_hold_forward,
);

decl_count_ctx!(
    increment_slot_share_count,
    decrement_slot_share_count,
    acquire_slot_share,
    release_slot_share,
    propagate_slot_share_forward,
);

decl_count_ctx!(
    increment_drop_hold_count,
    decrement_drop_hold_count,
    acquire_drop_hold,
    release_drop_hold,
    propagate_drop_hold_forward,
);

decl_count_ctx!(
    increment_slot_hold_count,
    decrement_slot_hold_count,
    acquire_slot_hold,
    release_slot_hold,
    propagate_slot_hold_forward,
);

decl_count_ctx!(
    increment_touch_count,
    decrement_touch_count,
    acquire_touch,
    release_touch,
    propagate_touch_forward,
);

decl_count_ctx!(
    increment_server_count,
    decrement_server_count,
    acquire_server,
    release_server,
    propagate_server_forward,
);

// Like `create`, but stamps the new row at a specific `time_ms` rather
// than `now`. Used by the action-completion path to materialize products
// at the action's scheduled completion time. Same definition-flag merge
// as `create`.
#[allow(clippy::too_many_arguments)]
pub fn create_at(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    surface: u8,
    macro_zone: u64,
    micro_zone: u8,
    micro_location: u32,
    owner_id: u32,
    packed_definition: u16,
    flags_state: u32,
    flags_bk: u32,
) -> Card {
    write_at(
        ctx,
        Card {
            valid_at: 0,
            card_id,
            surface,
            macro_zone,
            micro_zone,
            micro_location,
            owner_id,
            packed_definition,
            flags_state: flags_state | definition_state_flag_mask(packed_definition),
            flags_bk,
        },
        time_ms,
    )
}

/// Single-row counter table holding the next card_id to allocate.
/// Private — internal allocator state, not part of the client wire.
///
/// PK is always `0` — this is a one-row table; we use `id` as a
/// fixed sentinel rather than `#[auto_inc]` because we want stable
/// access to the same row across calls.
#[table(accessor = card_id_counter)]
pub struct CardIdCounter {
    #[primary_key]
    pub id: u8,
    pub next: u32,
}

/// Allocate a fresh card_id in O(1). Backed by a single-row counter
/// table; lazy-seeded from the current `max(card_id) + 1` on the very
/// first call after a fresh deployment, then pure read-modify-write
/// thereafter. Inserts within the current reducer are visible to
/// subsequent calls — three creates in a loop produce three distinct
/// ids.
///
/// Previously this scanned the whole cards table on every call —
/// O(N) over every version row — which became expensive as the
/// history grew. Counter table fixes that without changing semantics.
pub fn next_card_id(ctx: &ReducerContext) -> u32 {
    if let Some(counter) = ctx.db.card_id_counter().id().find(0) {
        let allocated = counter.next;
        // Delete-and-reinsert is the established pattern in this
        // codebase (see `cards::write_at`, `players::write_at`); avoids
        // depending on whether `.update` is exposed on this binding
        // version.
        ctx.db.card_id_counter().id().delete(0);
        ctx.db.card_id_counter().insert(CardIdCounter {
            id: 0,
            next: allocated.saturating_add(1),
        });
        allocated
    } else {
        // Lazy seed. One full scan, paid exactly once after each fresh
        // deployment (or after `republish` clears data). The seed must
        // include existing cards so we don't collide with rows the
        // counter wasn't tracking yet.
        let current_max = ctx
            .db
            .cards()
            .iter()
            .map(|c| c.card_id)
            .max()
            .unwrap_or(0);
        let allocated = current_max.saturating_add(1);
        ctx.db.card_id_counter().insert(CardIdCounter {
            id: 0,
            next: allocated.saturating_add(1),
        });
        allocated
    }
}

// ---- tile-card promotion ----------------------------------------------
//
// A zone tile is lazily promoted to a real `Card` row the first time
// the unified-hold-count machinery (or any card-shaped read site) needs
// to operate on it. Promotion is idempotent — concurrent accepts on
// the same hex resolve to the same card via [`find_tile_card_at`]
// before falling through to creation. Demotion (the reverse — folding
// an at-rest tile-card back into the zone's packed tile slot) lives in
// the GC sweep. See `docs/TILE_AS_CARD.md`.
//
// Disambiguation: zone tiles are hex-shaped cards whose `card_type`
// matches the Zone's `packed_definition`. Rect-shaped cards placed at
// the same hex live at `state = Free` carrying their own type; when a
// rect is present, the tile-card sits **beneath** it as
// `state = OnRoot` with `direction = STACK_DIR_HEX` (the slot.0
// branch). This inverts the legacy "hex is the anchor, rect chains
// on via OnHex" model — under the unified card model the rect is the
// root and the hex is its branch-0 child.

/// Snapshot of what currently occupies a hex, threaded through
/// promotion so the placement decision and the find can share one
/// macro_zone scan.
struct HexOccupancy {
    /// Existing tile-card at the hex, if any. Either standalone
    /// (state = Free) or stacked beneath a rect
    /// (state = OnRoot, direction = STACK_DIR_HEX).
    tile_card: Option<Card>,
    /// Existing Free-state non-tile root at the hex, if any. When
    /// `tile_card` is `None` and this is `Some`, a new tile-card
    /// promotes underneath as `state = OnRoot,
    /// direction = STACK_DIR_HEX, micro_location = rect.card_id`.
    rect_root: Option<Card>,
}

/// Single macro_zone scan that captures every signal
/// `find_or_create_tile_card` needs. Iterates the `macro_zone` btree,
/// dedupes by `card_id` (history rows produce multiple entries per
/// card_id), and resolves each card_id to its row current at
/// `time_ms` via [`prior_at`].
fn inspect_hex(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    tile_card_type: u8,
    time_ms: u64,
) -> HexOccupancy {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut free_tile: Option<Card> = None;
    let mut rect_root: Option<Card> = None;
    let mut tile_children: Vec<Card> = Vec::new();

    for c in ctx.db.cards().macro_zone().filter(macro_zone) {
        if !seen.insert(c.card_id) {
            continue;
        }
        let Some(latest) = prior_at(ctx, c.card_id, time_ms) else {
            continue;
        };
        if latest.surface != surface {
            continue;
        }
        let (cq, cr, state) = unpack_micro_zone(latest.micro_zone);
        let (card_type, _) = unpack_definition(latest.packed_definition);

        match state {
            StackedState::Free if cq == q && cr == r => {
                if card_type == tile_card_type {
                    free_tile = Some(latest);
                } else {
                    rect_root = Some(latest);
                }
            }
            StackedState::OnRoot
                if card_type == tile_card_type
                    && micro_zone_direction(latest.micro_zone) == STACK_DIR_HEX =>
            {
                tile_children.push(latest);
            }
            _ => {}
        }
    }

    // Resolve tile_card: a standalone Free tile-card wins outright.
    // Otherwise look for a tile-child whose `micro_location` points
    // back to the rect root. A child without a matching root is data
    // drift — leave `tile_card = None` so promotion either rebinds
    // under the current rect or creates fresh.
    let tile_card = if free_tile.is_some() {
        free_tile
    } else if let Some(root) = rect_root.as_ref() {
        tile_children
            .into_iter()
            .find(|c| c.micro_location == root.card_id)
    } else {
        None
    };

    HexOccupancy { tile_card, rect_root }
}

/// Look up an existing tile-card at `(surface, macro_zone, q, r)`
/// whose `card_type` matches `tile_card_type`. Checks both placements
/// (standalone Free at the hex, or OnRoot/STACK_DIR_HEX beneath a
/// rect). `time_ms` filters future-stamped rows so concurrent accepts
/// see a consistent snapshot.
pub fn find_tile_card_at(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    tile_card_type: u8,
    time_ms: u64,
) -> Option<Card> {
    inspect_hex(ctx, surface, macro_zone, q, r, tile_card_type, time_ms).tile_card
}

/// Promote a zone tile to a real Card row, or return the existing
/// tile-card if one is already present. Idempotent — concurrent
/// callers targeting the same hex resolve to the same card via
/// `inspect_hex` before reaching the create path.
///
/// On promotion the new card is seeded from the Zone's tile slot at
/// `(q, r)`:
///
/// - `packed_definition` = `pack_definition(zone_card_type, tile_def_id)`.
/// - `owner_id` = `Zone.owner_id` (world: 0; mini_zone / pocket: the
///   anchor's card_id).
/// - `flags_state` / `flags_bk` = `0`. Def-driven state flags are
///   merged in by [`create`] / [`create_at`] via
///   `definition_state_flag_mask`.
/// - Placement: standalone `Free` at the hex if no rect occupies it;
///   else `OnRoot` direction `STACK_DIR_HEX` position `1` with
///   `micro_location = rect.card_id` (the rect becomes its parent).
///
/// **Stock state.** The Zone's `(stock0, stock1)` are NOT carried
/// onto the seeded card in this phase — they remain readable from
/// the zone slot until the write-rerouting phase introduces a
/// per-card stock store. Read paths that need stock during the
/// promotion window should still consult `zone.tile_stock_at(...)`.
/// See `docs/TILE_AS_CARD.md` Phase 4.
///
/// Returns `Err` when no Zone is registered at `(surface,
/// macro_zone)`, when `(q, r)` is out of range, or when the zone
/// slot is empty (`def_id == 0`).
///
/// The created card's `owner_id` comes from the resolved Zone's
/// `owner_id`.
pub fn find_or_create_tile_card(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    time_ms: u64,
) -> Result<Card, String> {
    let zone = crate::zones::latest_for(ctx, surface, macro_zone).ok_or_else(|| {
        format!(
            "find_or_create_tile_card: no zone at (surface={surface}, macro_zone={macro_zone})"
        )
    })?;
    let tile_card_type = unpack_zone_definition(zone.packed_definition);

    let occupancy = inspect_hex(ctx, surface, macro_zone, q, r, tile_card_type, time_ms);
    if let Some(existing) = occupancy.tile_card {
        return Ok(existing);
    }

    // No existing tile-card — read the zone slot and create one.
    // `zone.tile_at` is (row, col): r is row, q is col.
    let (tile_def_id, stock0, stock1) = zone.tile_at(r, q).ok_or_else(|| {
        format!("find_or_create_tile_card: hex (q={q}, r={r}) out of range")
    })?;
    if tile_def_id == 0 {
        return Err(format!(
            "find_or_create_tile_card: zone slot at (q={q}, r={r}) is empty (def_id=0)"
        ));
    }
    let packed_def = pack_definition(tile_card_type, tile_def_id);

    let card_id = next_card_id(ctx);
    let (micro_zone_byte, micro_location) = match occupancy.rect_root.as_ref() {
        Some(rect) => (
            // Single child in the rect's slot.0 branch — position 1.
            pack_stack_micro_zone(1, STACK_DIR_HEX, StackedState::OnRoot),
            rect.card_id,
        ),
        None => (pack_micro_zone(q, r, StackedState::Free), 0),
    };

    // Seed flags_bk with stock from the zone slot. Demotion folds
    // these back into the zone tile slot; downstream mutations go
    // through `set_tile_stock`.
    let initial_flags_bk = {
        let mut bk = 0u32;
        bk = write_tile_stock(bk, 0, stock0);
        bk = write_tile_stock(bk, 1, stock1);
        bk
    };

    Ok(create_at(
        ctx,
        card_id,
        time_ms,
        surface,
        macro_zone,
        micro_zone_byte,
        micro_location,
        zone.owner_id,
        packed_def,
        0,
        initial_flags_bk,
    ))
}

// ---- tile-card stock accessors ---------------------------------------
//
// Stock values for promoted tile-cards live in two u2 fields in
// `flags_bk` (`tile_stock_0`, `tile_stock_1` — see
// `content/cards/flags.json`). Meaningless on non-tile cards. Stock
// writes are absolute (not delta) — no `propagate_*_forward` helper;
// the existing `prior_at` semantics give downstream readers the
// correct value at any time. See `docs/TILE_AS_CARD.md`.

/// Read `tile_stock_<slot>` (0 or 1) from a `flags_bk` u32. Returns
/// 0..=3 for in-range `slot`; returns 0 for any other `slot` (the
/// caller violated the 0..=1 contract — treated as "no stock").
pub fn tile_stock(flags_bk: u32, slot: usize) -> u8 {
    let b = bk_flags();
    let (mask, shift) = match slot {
        0 => (b.tile_stock_0_mask, b.tile_stock_0_shift),
        1 => (b.tile_stock_1_mask, b.tile_stock_1_shift),
        _ => return 0,
    };
    ((flags_bk & mask) >> shift) as u8
}

/// Pure-transform writer for `tile_stock_<slot>`. Replaces the slot's
/// bits in `flags_bk` with `value` (clamped to the field's u2 width).
/// Internal — exposed publicly only via [`set_tile_stock`] which
/// pairs the write with `update_with_at` discipline.
fn write_tile_stock(flags_bk: u32, slot: usize, value: u8) -> u32 {
    let b = bk_flags();
    let (mask, shift, max) = match slot {
        0 => (
            b.tile_stock_0_mask,
            b.tile_stock_0_shift,
            b.tile_stock_0_max,
        ),
        1 => (
            b.tile_stock_1_mask,
            b.tile_stock_1_shift,
            b.tile_stock_1_max,
        ),
        _ => return flags_bk,
    };
    (flags_bk & !mask) | (((value as u32) & max) << shift)
}

/// Set `tile_stock_<slot>` on a tile-card at `time_ms`. Wraps
/// [`write_tile_stock`] in `update_with_at` so the write inherits
/// `write_at`'s prior-row read + dirty/preserve diff discipline.
pub fn set_tile_stock(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    slot: usize,
    value: u8,
) -> Option<Card> {
    update_with_at(ctx, card_id, time_ms, |c| {
        c.flags_bk = write_tile_stock(c.flags_bk, slot, value);
    })
}

// ---- card-priority tile views ----------------------------------------
//
// The unified tile read API: every place that previously called
// `zone.tile_at(...)` / `zone.tile_def_id_at(...)` should route
// through these helpers so a promoted tile-card's data wins over the
// underlying zone slot. Cards' `packed_definition` carries
// `(card_type | def_id)` so a tile-card with a mutated def
// (Phase 4+) surfaces here without callers having to know about
// promotion.
//
// **Stock caveat.** Tile-cards don't carry stock until the Phase 4
// write-reroute lands. Until then `tile_full_view` returns stock
// from the Zone slot even when a tile-card is present. Callers that
// need post-Phase-4-correct stock should keep reading through these
// helpers — the stock source flips silently when Phase 4 lands.

/// Card-priority view of the tile def at `(surface, macro_zone, q,
/// r)`. Returns the promoted tile-card's def_id when one exists,
/// else the Zone slot's def_id. Returns `None` when no Zone is
/// registered or `(q, r)` is out of range.
pub fn tile_def_id_view(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    time_ms: u64,
) -> Option<u16> {
    let zone = crate::zones::latest_for(ctx, surface, macro_zone)?;
    let tile_card_type = unpack_zone_definition(zone.packed_definition);
    if let Some(tile) =
        find_tile_card_at(ctx, surface, macro_zone, q, r, tile_card_type, time_ms)
    {
        let (_, def_id) = unpack_definition(tile.packed_definition);
        return Some(def_id);
    }
    zone.tile_def_id_at(r, q)
}

/// Card-priority view of `(packed_def, stock0, stock1)` at
/// `(surface, macro_zone, q, r)`. `packed_def` comes from the
/// promoted tile-card when present (so card-side def mutations
/// surface), else from the Zone slot (constructed via
/// `pack_definition(zone_card_type, zone_def_id)`).
///
/// Returns `None` when no Zone is registered, `(q, r)` is out of
/// range, OR when there's no tile-card and the Zone slot is empty
/// (`def_id == 0`).
pub fn tile_full_view(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    time_ms: u64,
) -> Option<(u16, u8, u8)> {
    let zone = crate::zones::latest_for(ctx, surface, macro_zone)?;
    let tile_card_type = unpack_zone_definition(zone.packed_definition);

    if let Some(tile) =
        find_tile_card_at(ctx, surface, macro_zone, q, r, tile_card_type, time_ms)
    {
        // Card-resident def AND stock win — Phase 4's per-card stock
        // storage in `flags_bk.tile_stock_{0,1}` is canonical for
        // promoted tile-cards. Demotion folds these back into the
        // zone slot.
        let stock0 = tile_stock(tile.flags_bk, 0);
        let stock1 = tile_stock(tile.flags_bk, 1);
        return Some((tile.packed_definition, stock0, stock1));
    }

    let (zone_def_id, stock0, stock1) = zone.tile_at(r, q)?;
    if zone_def_id == 0 {
        return None;
    }
    Some((pack_definition(tile_card_type, zone_def_id), stock0, stock1))
}

// ---- single-field setters ---------------------------------------------

pub fn set_surface(ctx: &ReducerContext, card_id: u32, surface: u8) -> Option<Card> {
    update_with(ctx, card_id, |c| c.surface = surface)
}

pub fn set_macro_zone(ctx: &ReducerContext, card_id: u32, macro_zone: u64) -> Option<Card> {
    update_with(ctx, card_id, |c| c.macro_zone = macro_zone)
}

pub fn set_micro_zone(ctx: &ReducerContext, card_id: u32, micro_zone: u8) -> Option<Card> {
    update_with(ctx, card_id, |c| c.micro_zone = micro_zone)
}

pub fn set_micro_location(
    ctx: &ReducerContext,
    card_id: u32,
    micro_location: u32,
) -> Option<Card> {
    update_with(ctx, card_id, |c| c.micro_location = micro_location)
}

pub fn set_owner(ctx: &ReducerContext, card_id: u32, owner_id: u32) -> Option<Card> {
    update_with(ctx, card_id, |c| c.owner_id = owner_id)
}

/// Resolve the player who ultimately owns `card_id`.
///
/// Walks `owner_id` upward — each non-soul card's `owner_id` is the
/// next container card_id — until we hit a row with
/// [`FLAG_OWNED_BY_PLAYER`] set, at which point that row's
/// `owner_id` IS the player_id and we return it. World-owned chains
/// terminate at `owner_id == 0` and resolve to [`WORLD_PLAYER_ID`]
/// (= 0), so callers that want to special-case world ownership can
/// check the return value against 0 (or the constant) rather than
/// branching on `Option`.
///
/// Returns `None` only on malformed input: depth cap exceeded
/// (cycle that slipped past [`would_cycle`]'s writer-side check) or
/// an intermediate card_id with no `latest` row. Neither should
/// happen in well-formed state — they're surfaced as `None` rather
/// than panicking so reducer callers can degrade gracefully.
pub fn owning_player(ctx: &ReducerContext, card_id: u32) -> Option<u32> {
    let mut cur = card_id;
    for _ in 0..OWNER_WALK_DEPTH_CAP {
        let row = latest(ctx, cur)?;
        if row.flags_state & state_flags().is_owned_by_player != 0 {
            return Some(row.owner_id);
        }
        if row.owner_id == 0 {
            return Some(WORLD_PLAYER_ID);
        }
        cur = row.owner_id;
    }
    None
}

/// Like [`owning_soul`] but bounded by `time_ms` instead of `now`.
/// Use this from inside chained apply paths where the actor card
/// was just spawned at a future-stamped `valid_at = time_ms`:
/// `latest` would look up rows at `now < time_ms` and return `None`,
/// breaking owner resolution. Pass the action's `completion_ms` to
/// see the actor's most-recent row at the action's frame of
/// reference instead.
pub fn owning_soul_at(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
) -> Option<u32> {
    let mut cur = card_id;
    for _ in 0..OWNER_WALK_DEPTH_CAP {
        let row = prior_at(ctx, cur, time_ms)?;
        if row.flags_state & state_flags().is_owned_by_player != 0 {
            return Some(cur);
        }
        if row.owner_id == 0 {
            return None;
        }
        cur = row.owner_id;
    }
    None
}

/// Resolve which soul card ultimately contains `card_id`.
///
/// Walks `owner_id` upward until it hits a row carrying
/// [`FLAG_OWNED_BY_PLAYER`] — that row IS the soul, and its
/// `card_id` is returned. If `card_id` itself is a soul, returns
/// `card_id` directly.
///
/// Returns `None` when the walk reaches world (`owner_id == 0`)
/// without finding a soul, or on malformed input (cycle / missing
/// intermediate). Callers use the `None` case to decide "this card
/// has no soul context" (world-owned cards, etc.) — typically
/// shortcircuiting has-predicate matching to an empty stack.
pub fn owning_soul(ctx: &ReducerContext, card_id: u32) -> Option<u32> {
    let mut cur = card_id;
    for _ in 0..OWNER_WALK_DEPTH_CAP {
        let row = latest(ctx, cur)?;
        if row.flags_state & state_flags().is_owned_by_player != 0 {
            return Some(cur);
        }
        if row.owner_id == 0 {
            return None;
        }
        cur = row.owner_id;
    }
    None
}

/// Resolve which card's inventory bucket `card_id` lives in.
///
/// - If `card_id` IS a soul (carries `FLAG_OWNED_BY_PLAYER`) →
///   returns `card_id` itself. Souls are their own bucket: the
///   soul's `card_id` is the macro_zone of its inventory.
/// - Otherwise → returns the row's `owner_id`, which is the
///   container card_id (or `0` for world-owned cards).
///
/// Used by recipe product placement to spawn a card into the same
/// inventory as the actor / hex / root role, without each call site
/// having to special-case "is this a soul." Returns `0` when the
/// card_id has no `latest` row (mirrors the existing
/// `unwrap_or(0)` discipline at the resolver call sites).
pub fn inventory_container_for(ctx: &ReducerContext, card_id: u32) -> u32 {
    let Some(row) = latest(ctx, card_id) else {
        return 0;
    };
    if row.flags_state & state_flags().is_owned_by_player != 0 {
        card_id
    } else {
        row.owner_id
    }
}

/// Like [`inventory_container_for`] but bounded by `time_ms` instead
/// of `now`. Same use case as [`owning_soul_at`]: chained on-create
/// applies whose actor was just spawned at a future-stamped
/// `valid_at = time_ms` won't be visible to `latest`, but
/// `prior_at(..., time_ms)` finds them.
pub fn inventory_container_for_at(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
) -> u32 {
    let Some(row) = prior_at(ctx, card_id, time_ms) else {
        return 0;
    };
    if row.flags_state & state_flags().is_owned_by_player != 0 {
        card_id
    } else {
        row.owner_id
    }
}

/// Pre-flight cycle check for any reducer about to set
/// `card_id.owner_id = new_owner_card_id`. Returns true iff making
/// that assignment would create a cycle reachable by walking
/// `owner_id` upward.
///
/// Walks up from `new_owner_card_id`; if we visit `card_id`, the
/// proposed assignment would form a loop (a child trying to claim
/// one of its own ancestors as its container). Also returns true if
/// the depth cap trips, treating "too deep to verify" as unsafe.
///
/// Stops at the world boundary (`owner_id == 0`) and at soul cards
/// (`FLAG_OWNED_BY_PLAYER` set) — neither continues the card-id
/// chain, so neither can reach back to `card_id`.
///
/// Callers should reject the operation when this returns true.
pub fn would_cycle(ctx: &ReducerContext, card_id: u32, new_owner_card_id: u32) -> bool {
    if new_owner_card_id == 0 {
        return false;
    }
    if new_owner_card_id == card_id {
        return true;
    }
    let mut cur = new_owner_card_id;
    for _ in 0..OWNER_WALK_DEPTH_CAP {
        let Some(row) = latest(ctx, cur) else {
            return false;
        };
        if row.flags_state & state_flags().is_owned_by_player != 0 {
            return false;
        }
        if row.owner_id == 0 {
            return false;
        }
        if row.owner_id == card_id {
            return true;
        }
        cur = row.owner_id;
    }
    true
}

pub fn set_packed_definition(
    ctx: &ReducerContext,
    card_id: u32,
    packed_definition: u16,
) -> Option<Card> {
    update_with(ctx, card_id, |c| c.packed_definition = packed_definition)
}

pub fn set_flags_state(ctx: &ReducerContext, card_id: u32, flags_state: u32) -> Option<Card> {
    update_with(ctx, card_id, |c| c.flags_state = flags_state)
}
