use spacetimedb::{table, ReducerContext, Table};

use crate::flags::bk_flags;
use crate::packed::{pack_valid_at, valid_at_time, STACK_STATE_DEFERRED};
use crate::sequence;
use resonantdust_codec::card_model::{
    decrement_hold, hold_count, increment_hold, is_dead, placement_mask, state_mask, HoldField,
};

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

// ── refcount holds ───────────────────────────────────────────────────────
//
// All six count fields (touch / slot_claim / slot_borrow / position_hold /
// drop_hold / server) live in the propagating `flags` word; the bit math is
// owned by `resonantdust_codec::card_model`. These thin readers plus the generic
// acquire / release / propagate machinery wrap that arithmetic with the
// cards-table mutation and forward-prop walk.

/// Read the `slot_claim` (exclusive) hold count from a `flags` word.
pub fn slot_claim_count(flags: u32) -> u8 {
    hold_count(flags, HoldField::SlotClaim)
}
/// Read the `slot_borrow` (shared) hold count from a `flags` word.
pub fn slot_borrow_count(flags: u32) -> u8 {
    hold_count(flags, HoldField::SlotBorrow)
}
/// Read the `position_hold` count from a `flags` word.
pub fn position_hold_count(flags: u32) -> u8 {
    hold_count(flags, HoldField::PositionHold)
}
/// Read the `touch` count from a `flags` word.
pub fn touch_count(flags: u32) -> u8 {
    hold_count(flags, HoldField::Touch)
}
/// Read the `server` count from a `flags` word.
pub fn server_count(flags: u32) -> u8 {
    hold_count(flags, HoldField::Server)
}
/// Read the `drop_hold` (stacking-block) count from a `flags` word.
pub fn drop_hold_count(flags: u32) -> u8 {
    resonantdust_codec::card_model::drop_hold_count(flags)
}

/// Acquire one reference of `field` on a card at `time_ms` — bumps the count on
/// the row current at that time AND forward-propagates +1 onto every
/// future-stamped row (so a future release row, with the count baked in,
/// correctly reflects "but someone else is still holding" once this lands).
pub fn acquire_hold(ctx: &ReducerContext, card_id: u32, time_ms: u64, field: HoldField) {
    update_with_at(ctx, card_id, time_ms, |c| {
        c.flags = increment_hold(c.flags, field);
    });
    propagate_hold_forward(ctx, card_id, time_ms, field, true);
}

/// Release one reference of `field` on a card at `time_ms` — decrements at
/// `time_ms` and on every future-stamped row of the same card.
pub fn release_hold(ctx: &ReducerContext, card_id: u32, time_ms: u64, field: HoldField) {
    update_with_at(ctx, card_id, time_ms, |c| {
        c.flags = decrement_hold(c.flags, field);
    });
    propagate_hold_forward(ctx, card_id, time_ms, field, false);
}

/// Apply ±1 to `field` on every row of this card with `valid_at_time > time_ms`.
/// Bypasses `write_at` (direct `delete` / `insert`) so we don't re-fire the
/// `souls::on_card_write` / lifecycle hooks while rewriting rows we already
/// wrote. The `valid_at` PK is preserved across each delete/insert pair.
fn propagate_hold_forward(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    field: HoldField,
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
        let new_flags = if increment {
            increment_hold(row.flags, field)
        } else {
            decrement_hold(row.flags, field)
        };
        ctx.db.cards().valid_at().delete(v);
        let mut updated = row;
        updated.flags = new_flags;
        ctx.db.cards().insert(updated);
    }
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
    /// Dual-interpretation, gated by the `stack` field in `flags` (bits 0-3);
    /// `stack == 0` is the loose sentinel:
    /// - `stack != 0` → **root card_id**. This card is a stack member; its branch
    ///   is `stack - 1` and its slot the `index` field. The btree index makes
    ///   "all members of root R" a single `micro_location().filter(R)` — the
    ///   lookup the whole flat-chain model relies on. Deferred members (branch ==
    ///   `STACK_STATE_DEFERRED`) carry their host's id here and are tracked by
    ///   `state_3_followers`.
    /// - `stack == 0` → **loose coords + offset** `[local_q:3 | local_r:3 | x:12
    ///   | y:12 | rsvd:2]` (see `packed::pack_micro_loose`); the loose kind lives
    ///   in the `index` field.
    #[index(btree)]
    pub micro_location: u32,
    #[index(btree)]
    pub owner_id: u32,
    pub packed_definition: u16,
    /// The **propagating** flag word: gameplay state bits, placement
    /// (`stack`/`index`), and the refcount holds. State bits are bit-diff
    /// propagated forward by [`propagate_flag_diff_forward`]; the refcounts are
    /// delta-propagated by [`propagate_hold_forward`]; placement is per-row.
    /// Bit layout lives in `resonantdust_codec::flags`'s `flags` section and is
    /// surfaced via [`crate::flags::state_flags`] + `card_model`.
    pub flags: u32,
    /// Non-propagating bookkeeping byte — the server-managed dirty / preserve
    /// markers, recomputed by [`write_at`] on every insert and never carried
    /// forward. Bit layout: `resonantdust_codec::flags`'s `flags_bk` section
    /// ([`crate::flags::bk_flags`]).
    pub flags_bk: u8,
    /// Per-card variable data — a full `u64`, packed however the card def wants
    /// (32 u2s, 8 u8s, a u8 progress counter + lock aspects, …). Only the bottom
    /// u4 (`card_model::STOCK_ZONE_SAVE_MASK`) can be persisted back to a zone tile
    /// slot; the upper 60 bits are card-only (transient unless the card persists).
    /// Initialized from the def's stock default on spawn. Zero by default.
    pub stock: u64,
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
pub use resonantdust_codec::card_model::Micro;

/// Extension adapting the shared [`Micro::apply`] (raw `flags_bk` → `(micro_location,
/// flags_bk)`) to write directly onto a `Card` row, preserving the `m.place(&mut c)`
/// call shape this module used before the model moved to `content`.
pub trait MicroPlace {
    /// Write this placement onto `card`: sets `micro_location` and the
    /// `stack` / `index` fields in `flags` (preserving every other `flags` bit —
    /// state + refcounts).
    fn place(self, card: &mut Card);
}

impl MicroPlace for Micro {
    fn place(self, card: &mut Card) {
        let (micro_location, flags) = self.apply(card.flags);
        card.micro_location = micro_location;
        card.flags = flags;
    }
}

/// Decode a card row's current micro placement (the `&Card` adapter over the
/// shared [`Micro::of`]).
pub fn micro_of(card: &Card) -> Micro {
    Micro::of(card.micro_location, card.flags)
}

/// True when `micro_location` is a root card_id (the card is a stack member).
pub fn micro_is_card(card: &Card) -> bool {
    resonantdust_codec::card_model::micro_is_card(card.flags)
}

/// The stack `branch` value (gated on [`micro_is_card`]).
pub fn stack_branch(card: &Card) -> u8 {
    resonantdust_codec::card_model::stack_branch(card.flags)
}

/// The `index` slot value (only meaningful when [`micro_is_card`]).
pub fn stack_index(card: &Card) -> u8 {
    resonantdust_codec::card_model::stack_index(card.flags)
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
                && crate::packed::is_player_soul(c.packed_definition)
                && !is_dead(c.flags)
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
///  - **Server forward-grace:** NONE. The client runs ≥1.5s behind true
///    server time (`client_delay`), so a `client_time_ms` ahead of the
///    server's clock is never legitimate jitter — it's genuine skew, and
///    accepting it would stamp the row in the server's future where a
///    correctly-clocked observer can't see it yet (the bug that left
///    neighbour regions un-materialized). So ANY ahead is rejected as
///    `time_drift:client_ahead`; the client re-seats its clock and
///    re-requests (it never clamps — a future stamp is a real signal).
///
/// Set to 0: there is no forward window. (Back-grace is the separate
/// `BACKWARD_GRACE_MS` + `MAX_RTT_MS`.) This is the single forward knob —
/// it must stay 0 unless the client's behind-clock invariant changes.
pub const TIME_DRIFT_BUFFER_MS: u64 = 0;

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
    // (anti-cheat, not lag absorption). The client runs behind (clientDelay),
    // so any meaningful "ahead" is skew — surfaced, not clamped.
    let ahead = client_time_ms.saturating_sub(server);
    if ahead > TIME_DRIFT_BUFFER_MS {
        return Err(format!(
            "time_drift:client_ahead_by={ahead} (server={server}, client={client_time_ms})"
        ));
    }
    // Stamp at the client's (behind) time — no `min(client, server)` clamp.
    // Clamping a within-grace forward overshoot to server-time would land
    // completion rows off the client's timeline; reject-beyond-grace is the
    // only future handling.
    Ok(client_time_ms)
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

/// The first unoccupied snap cell in `macro_zone` (row-major over the 8×8 zone)
/// at `time_ms` — for placing a new snapped card ONE-PER-TILE (every surface is a
/// uniform hex grid; snapped cards carry a zero offset). Occupants are the live
/// loose cards currently in the zone. Falls back to `(0, 0)` if the zone is full
/// (degenerate, but keeps placement total).
/// Cube/hex distance between axial tile coords. Mirrors `regions::hex_dist`.
fn hex_dist(aq: i32, ar: i32, bq: i32, br: i32) -> i32 {
    let dq = aq - bq;
    let dr = ar - br;
    (dq.abs() + dr.abs() + (dq + dr).abs()) / 2
}

pub fn first_free_cell(ctx: &ReducerContext, macro_zone: u64, distance: u16, time_ms: u64) -> (u8, u8) {
    use crate::packed::{world_tile, ZONE_SIZE};
    let (zq, zr) = crate::packed::unpack_macro_zone(macro_zone);
    let d = distance as i32;
    let mut occupied = std::collections::BTreeSet::new();
    let mut seen = std::collections::BTreeSet::new();
    for row in ctx.db.cards().macro_zone().filter(macro_zone) {
        if !seen.insert(row.card_id) {
            continue;
        }
        let Some(card) = prior_at(ctx, row.card_id, time_ms) else {
            continue;
        };
        if is_dead(card.flags) {
            continue;
        }
        if let Micro::Loose { local_q, local_r, .. } = Micro::of(card.micro_location, card.flags) {
            occupied.insert((local_q, local_r));
        }
    }
    for r in 0..ZONE_SIZE {
        for q in 0..ZONE_SIZE {
            // Skip cells outside the region's disk — those tiles don't exist
            // (the gate never spawns them). `distance = u16::MAX` ⇒ unbounded.
            if hex_dist(world_tile(zq, q as u8), world_tile(zr, r as u8), 0, 0) > d {
                continue;
            }
            if !occupied.contains(&(q as u8, r as u8)) {
                return (q as u8, r as u8);
            }
        }
    }
    (0, 0)
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
            // `macro_zone` encodes surface (bits 24-31), so its diff covers a
            // surface change too. Placement (`stack` / `index`, in `flags`) is
            // part of the position tuple — a re-stack / re-index is a position
            // change even when `micro_location` (the root pointer) is unchanged.
            let placement = placement_mask();
            let pos_changed = card.macro_zone != prev.macro_zone
                || card.micro_location != prev.micro_location
                || (card.flags & placement) != (prev.flags & placement);
            // Data diff keys on owner / def / the bit-diff state bits only. The
            // refcounts and placement also live in `flags`, so mask to
            // `state_mask()` — they must not register as a data change here (the
            // refcounts have their own delta propagator; placement is position).
            let sm = state_mask();
            let data_changed = card.owner_id != prev.owner_id
                || card.packed_definition != prev.packed_definition
                || (card.flags & sm) != (prev.flags & sm);
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
    crate::souls::on_card_write(ctx, &inserted, time_ms);
    if let Some(prev) = prev_latest.as_ref() {
        propagate_flag_diff_forward(ctx, &inserted, prev.flags, time_ms);
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
    let position_changed = match prev {
        // `macro_zone` encodes surface, so this covers a surface move too.
        Some(p) => p.macro_zone != new.macro_zone,
        None => false, // first row for this card — no followers can be anchored yet.
    };
    let became_dead = match prev {
        Some(p) => !is_dead(p.flags) && is_dead(new.flags),
        None => is_dead(new.flags),
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
    prev_flags: u32,
    time_ms: u64,
) {
    // Only the bit-diff-propagated state bits participate. The refcount holds
    // (their own delta propagator) and placement (`stack`/`index`, per-row) also
    // live in `flags`, so mask down to `state_mask()` — they must not be
    // bit-diff-carried forward.
    let sm = state_mask();
    let new_state = new_card.flags & sm;
    let prev_state = prev_flags & sm;
    let set_bits = new_state & !prev_state;
    let clear_bits = prev_state & !new_state;
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
        let row_state = row.flags & sm;

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

        // Apply to the full `flags` word; `active_set`/`active_clear` are already
        // confined to `sm`, so refcounts/placement bits are untouched.
        let new_flags = (row.flags & !active_clear) | active_set;
        ctx.db.cards().valid_at().delete(v);
        let mut updated = row;
        updated.flags = new_flags;
        ctx.db.cards().insert(updated);
    }
}

// Insert a brand-new card. valid_at is computed; pass 0 will be overwritten.
//
// `flags` is the propagating word — pass the state bits only; placement
// (`stack`/`index`) is supplied via `micro` and written by `micro.place`.
// `stock` is the tile-card stock byte (0 for non-tile cards). Bookkeeping
// (`flags_bk`) starts at 0 and is managed by `write_at`. Most callers pass
// `flags = 0` (a player_soul is identified by its definition, not a flag).
#[allow(clippy::too_many_arguments)]
pub fn create(
    ctx: &ReducerContext,
    card_id: u32,
    macro_zone: u64,
    micro: Micro,
    owner_id: u32,
    packed_definition: u16,
    flags: u32,
    stock: u64,
) -> Card {
    let mut card = Card {
        valid_at: 0,
        card_id,
        macro_zone,
        micro_location: 0,
        owner_id,
        packed_definition,
        flags,
        flags_bk: 0,
        stock,
    };
    // Writes `micro_location` + the `stack`/`index` placement bits onto `flags`
    // (preserving the caller's state bits).
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
    flags: u32,
    stock: u64,
) -> Card {
    let mut card = Card {
        valid_at: 0,
        card_id,
        macro_zone,
        micro_location: 0,
        owner_id,
        packed_definition,
        flags,
        flags_bk: 0,
        stock,
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
    use crate::packed::{card_local_of, pack_card_id, CARD_LOCAL_MASK};

    // The counter holds the next *local* id (low 20 bits). The database
    // selector and shard id come from this deployment's `ShardIdentity` — the
    // unified module runs on both the owner-card DBs and the region DBs, so the
    // database bit isn't a compile-time constant. Composed on the way out, so a
    // card's id always names its own database + shard and the gateway routes by
    // it with no index lookup.
    let (card_db, shard) = identity(ctx);
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
    pack_card_id(card_db, shard, next_local)
}

/// One-row deployment identity: which database family this instance belongs to
/// (`CARD_DB_CARDS` / `CARD_DB_REGIONS`) and its shard id. The unified data
/// module is published to BOTH the owner-card DBs and the region DBs from one
/// binary; this row is how that single binary stamps `card_id`s with the
/// correct database bit. Seeded once per deployment by the gate
/// ([`crate::gate_api::set_shard_identity`]); when unseeded it defaults to the
/// card-DB family at [`crate::DATA_SHARD`], so a plain card shard works with no
/// seeding at all (only region DBs must be told their identity).
#[table(accessor = shard_identity)]
pub struct ShardIdentity {
    #[primary_key]
    pub id: u8,
    pub card_db: u8,
    pub shard: u16,
}

/// This deployment's `(card_db, shard)` — the seeded [`ShardIdentity`] row, or
/// the `(CARD_DB_CARDS, DATA_SHARD)` default when unseeded.
pub fn identity(ctx: &ReducerContext) -> (u8, u16) {
    match ctx.db.shard_identity().id().find(0) {
        Some(row) => (row.card_db, row.shard),
        None => (crate::packed::CARD_DB_CARDS, crate::DATA_SHARD),
    }
}

/// Seed (or overwrite) this deployment's identity. Gate-called once after
/// publish — delete-and-reinsert the single row (the established same-row
/// rewrite pattern; avoids depending on `.update`).
pub fn set_identity(ctx: &ReducerContext, card_db: u8, shard: u16) {
    ctx.db.shard_identity().id().delete(0);
    ctx.db
        .shard_identity()
        .insert(ShardIdentity { id: 0, card_db, shard });
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
        if crate::packed::is_player_soul(row.packed_definition) {
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
        if crate::packed::is_player_soul(row.packed_definition) {
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
        if crate::packed::is_player_soul(row.packed_definition) {
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
    if crate::packed::is_player_soul(row.packed_definition) {
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
    if crate::packed::is_player_soul(row.packed_definition) {
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
        if crate::packed::is_player_soul(row.packed_definition) {
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

pub fn set_flags(ctx: &ReducerContext, card_id: u32, flags: u32) -> Option<Card> {
    update_with(ctx, card_id, |c| c.flags = flags)
}
