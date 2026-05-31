use resonantdust_content::definition_core::decode_definition;
use spacetimedb::{table, ReducerContext, Table};

use crate::flags::{bk_flags, state_flags};
use crate::packed::{pack_valid_at, valid_at_time, STACK_STATE_DEFERRED};
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
/// `delete` / `insert`) so we don't recursively fire the
/// `souls::on_card_write` / lifecycle hooks while rewriting rows we
/// already wrote. `valid_at` PKs are preserved across the
/// delete/insert pair so the row's identity in history is unchanged.
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
    /// Packed location key `[reserved:u32 | surface:u8 | payload:u24]`.
    /// `surface` is bits 24-31 (read via `packed::surface_of`); no separate
    /// surface column. Payload is world `(zone_q, zone_r)` or a container id.
    #[index(btree)]
    pub macro_zone: u64,
    /// Dual-interpretation, gated by the `micro_is_card` flag (in `flags_bk`):
    /// - set   → **root card_id**. This card is a stack member; its branch is
    ///   the `stack_state` flag and its slot the `stack_index` flag. The btree
    ///   index makes "all members of root R" a single `micro_location().filter(R)`
    ///   — the lookup the whole flat-chain model relies on. Deferred members
    ///   (`stack_state == STACK_STATE_DEFERRED`) carry their host's id here and
    ///   are tracked by `state_3_followers`.
    /// - clear → **loose coords + offset** `[local_q:3 | local_r:3 | x:12 |
    ///   y:12 | rsvd:2]` (see `packed::pack_micro_loose`).
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

/// A card's micro placement is the **shared** [`content::card_model::Micro`]
/// model — the value of `micro_location` plus the `micro_is_card` /
/// `stack_state` / `stack_index` flag bits, written together so the
/// discriminator and the value it gates never drift. Re-exported here (not
/// duplicated) so `cards` and `regions` build their `Card` tables over one
/// model. The constructors (`Micro::snap`, `Micro::deferred`, `Micro::Stacked`,
/// `Micro::Loose`) and decode (`Micro::of(micro_location, flags_bk)`) come from
/// there; the [`MicroPlace`] extension below adapts the raw-field `apply` to
/// this module's `Card` row.
pub use resonantdust_content::card_model::Micro;

/// Extension adapting the shared [`Micro::apply`] (raw `flags_bk` → `(micro_location,
/// flags_bk)`) to write directly onto a `Card` row, preserving the `m.place(&mut c)`
/// call shape this module used before the model moved to `content`.
pub trait MicroPlace {
    /// Write this placement onto `card`: sets `micro_location` and the
    /// `micro_is_card` / `stack_state` / `stack_index` bits in `flags_bk`
    /// (preserving every other `flags_bk` bit).
    fn place(self, card: &mut Card);
}

impl MicroPlace for Micro {
    fn place(self, card: &mut Card) {
        let (micro_location, flags_bk) = self.apply(card.flags_bk);
        card.micro_location = micro_location;
        card.flags_bk = flags_bk;
    }
}

/// Decode a card row's current micro placement (the `&Card` adapter over the
/// shared [`Micro::of`]).
pub fn micro_of(card: &Card) -> Micro {
    Micro::of(card.micro_location, card.flags_bk)
}

/// True when `micro_location` is a root card_id (the card is a stack member).
pub fn micro_is_card(card: &Card) -> bool {
    resonantdust_content::card_model::micro_is_card(card.flags_bk)
}

/// The `stack_state` branch/kind value (gated on [`micro_is_card`]).
pub fn stack_branch(card: &Card) -> u8 {
    resonantdust_content::card_model::stack_branch(card.flags_bk)
}

/// The `stack_index` slot value (only meaningful when [`micro_is_card`]).
pub fn stack_index(card: &Card) -> u8 {
    resonantdust_content::card_model::stack_index(card.flags_bk)
}

/// The root card_id a stack member points at (`0` if the card is loose).
pub fn root_of_member(card: &Card) -> u32 {
    if micro_is_card(card) { card.micro_location } else { 0 }
}

/// Count `player_id`'s live, directly-owned souls: distinct cards whose LATEST
/// version has `owner_id == player_id`, the `is_owned_by_player` flag set, and
/// isn't `dead`. Only **player-souls** are owned by a `player_id` directly (the
/// world soul + loadout are owned by *cards*), so this is the player's
/// player-soul count — used to gate the non-idempotent starter spawn so a
/// re-login can't mint a duplicate.
pub fn count_player_souls(ctx: &ReducerContext, player_id: u32) -> usize {
    let s = state_flags();
    let mut latest: std::collections::HashMap<u32, Card> = std::collections::HashMap::new();
    for c in ctx.db.cards().owner_id().filter(player_id) {
        let newer = latest
            .get(&c.card_id)
            .map_or(true, |p| valid_at_time(c.valid_at) >= valid_at_time(p.valid_at));
        if newer {
            latest.insert(c.card_id, c);
        }
    }
    latest
        .values()
        .filter(|c| {
            c.owner_id == player_id
                && c.flags_state & s.is_owned_by_player != 0
                && c.flags_state & s.dead == 0
        })
        .count()
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
// exact key (two writes in the same ms), the existing one is replaced —
// "always accept the most recent write". Stale older rows for this card_id
// are reaped by the periodic GC sweep, not a per-write schedule.
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
            // `macro_zone` encodes surface (bits 24-31), so its diff covers
            // a surface change too. The stacking bits (`micro_is_card` /
            // `stack_state` / `stack_index`, in `flags_bk`) are part of the
            // position tuple — a re-stack / re-index is a position change even
            // when `micro_location` (the root pointer) is unchanged.
            let stack_mask = bk.micro_is_card | bk.stack_state_mask | bk.stack_index_mask;
            let pos_changed = card.macro_zone != prev.macro_zone
                || card.micro_location != prev.micro_location
                || (card.flags_bk & stack_mask) != (prev.flags_bk & stack_mask);
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
    inserted
}

/// Every card currently deferred (`micro_is_card` set, `stack_state ==
/// STACK_STATE_DEFERRED`) whose
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
        if !(micro_is_card(&latest) && stack_branch(&latest) == STACK_STATE_DEFERRED) {
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
        // `macro_zone` encodes surface, so this covers a surface move too.
        Some(p) => p.macro_zone != new.macro_zone,
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
            // Host moved zones — pull the follower along. `macro_zone`
            // carries the surface band, so this single field suffices.
            update_with_at(ctx, follower.card_id, time_ms, |c| {
                c.macro_zone = new.macro_zone;
            });
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
/// `delete` / `insert`) so we don't recursively fire the
/// `souls::on_card_write` / lifecycle hooks while rewriting rows we
/// already wrote. The `valid_at` PK is preserved across the
/// delete/insert pair, so the row's identity in history is unchanged.
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
    macro_zone: u64,
    micro: Micro,
    owner_id: u32,
    packed_definition: u16,
    flags_state: u32,
    flags_bk: u32,
) -> Card {
    let mut card = Card {
        valid_at: 0,
        card_id,
        macro_zone,
        micro_location: 0,
        owner_id,
        packed_definition,
        flags_state: flags_state | definition_state_flag_mask(packed_definition),
        flags_bk,
    };
    // Writes `micro_location` + the `micro_is_card`/`stack_state`/`stack_index`
    // bits onto `flags_bk` (OR'd over the caller's non-stack bits).
    micro.place(&mut card);
    write(ctx, card)
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
    macro_zone: u64,
    micro: Micro,
    owner_id: u32,
    packed_definition: u16,
    flags_state: u32,
    flags_bk: u32,
) -> Card {
    let mut card = Card {
        valid_at: 0,
        card_id,
        macro_zone,
        micro_location: 0,
        owner_id,
        packed_definition,
        flags_state: flags_state | definition_state_flag_mask(packed_definition),
        flags_bk,
    };
    micro.place(&mut card);
    write_at(ctx, card, time_ms)
}

/// Single-row counter table holding the next card_id to allocate.
/// Private — internal allocator state, not part of the client wire.
///
/// PK is always `0` — this is a one-row table; we use `id` as a
/// fixed sentinel rather than `#[auto_inc]` because we want stable
/// access to the same row across calls.
/// First `card_id` `next_card_id` will hand out on a fresh deployment. Ids
/// `0..FIRST_CARD_ID` are reserved for system / sentinel use — notably
/// `macro_zone`'s owner band uses `0` as the WORLD sentinel. Mirrors
/// `players::FIRST_PLAYER_ID`.
///
/// Player-soul cards are allocated normally (`next_card_id`) and identified
/// by `owner_id == player_id` + the `is_owned_by_player` flag, NOT by a
/// reserved id band — a player can own several (multi-character).
pub const FIRST_CARD_ID: u32 = 1024;

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
    use resonantdust_content::packed::{
        card_local_of, pack_card_id, CARD_DB_CARDS, CARD_LOCAL_MASK,
    };

    // The counter holds the next *local* id (low 20 bits). The database selector
    // (`CARD_DB_CARDS` = 0, the top id bit) and shard id (`DATA_SHARD`) are
    // composed on the way out, so a card's id always names its own database +
    // shard and the gateway routes by it with no index lookup.
    let next_local = match ctx.db.card_id_counter().id().find(0) {
        Some(counter) => {
            // Delete-and-reinsert is the established pattern here
            // (see `cards::write_at`); avoids depending on `.update`.
            ctx.db.card_id_counter().id().delete(0);
            counter.next
        }
        None => {
            // Lazy seed, paid once after a fresh deployment / republish: scan
            // the *local* part of existing card ids (all this shard's own rows)
            // so we don't collide with untracked rows. Locals `0..FIRST_CARD_ID`
            // stay reserved per shard (sentinels — `macro_zone` owner `0` =
            // WORLD).
            ctx.db
                .cards()
                .iter()
                .map(|c| card_local_of(c.card_id))
                .max()
                .unwrap_or(0)
                .saturating_add(1)
                .max(FIRST_CARD_ID)
        }
    };
    ctx.db.card_id_counter().insert(CardIdCounter {
        id: 0,
        // Cap at the 20-bit local ceiling — a per-shard backstop far above any
        // real working set (~1M cards/shard).
        next: next_local.saturating_add(1).min(CARD_LOCAL_MASK),
    });
    pack_card_id(CARD_DB_CARDS, crate::DATA_SHARD, next_local)
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
