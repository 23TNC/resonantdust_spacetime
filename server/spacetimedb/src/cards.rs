use resonantdust_content::definition_core::decode_definition;
use spacetimedb::{table, ReducerContext, Table};

use crate::packed::{pack_valid_at, valid_at_time};
use crate::sequence;

// Flag bit positions from `cards/flags.json`. Kept local for the
// dirty / preserve marker machinery that lives in this file; other
// modules duplicate the bits they need (see `action_completion.rs` /
// `actions.rs` for the holds and progress fields).
//
// `FLAG_POSITION_DIRTY` / `FLAG_DATA_DIRTY` are auto-set by `write_at`
// on every insert via a diff against `prev_latest` — callers should
// never set them manually.
//
// `FLAG_POSITION_PRESERVE` / `FLAG_DATA_PRESERVE` are caller-set
// intent flags: "don't override this row's position/data via forward
// propagation." The forward-prop helpers respect them as stop /
// skip conditions.
const FLAG_POSITION_DIRTY: u32 = 1 << 13;
const FLAG_POSITION_PRESERVE: u32 = 1 << 14;
const FLAG_DATA_DIRTY: u32 = 1 << 15;
const FLAG_DATA_PRESERVE: u32 = 1 << 16;

/// `position_hold_count` (bits 17..=19). 3-bit reference count of how
/// many distinct holders currently claim a position hold on this card
/// — chain stitches, magnetic pulls, has-predicate matches, etc.
/// Readers asking "is this card position-held?" check `count > 0`;
/// there's no separate `position_hold` flag bit anymore (bit 0 is a
/// tombstone in flags.json), the count IS the source of truth.
/// Saturates at `0b111 = 7`; in realistic play the count tops out
/// around 2-3 so the cap is comfortable slack and saturation only
/// ever errs on the "hold lingers a bit longer" side, never the
/// inverse.
const POSITION_HOLD_COUNT_SHIFT: u32 = 17;
pub const POSITION_HOLD_COUNT_MASK: u32 = 0b111 << POSITION_HOLD_COUNT_SHIFT;
const POSITION_HOLD_COUNT_MAX: u32 = 0b111;

/// Read the `position_hold_count` field out of a flags u32.
pub fn position_hold_count(flags: u32) -> u32 {
    (flags >> POSITION_HOLD_COUNT_SHIFT) & POSITION_HOLD_COUNT_MAX
}

/// Replace the `position_hold_count` field on a flags u32 with `count`.
fn write_position_hold_count(flags: u32, count: u32) -> u32 {
    (flags & !POSITION_HOLD_COUNT_MASK)
        | ((count & POSITION_HOLD_COUNT_MAX) << POSITION_HOLD_COUNT_SHIFT)
}

/// Pure flag transform: bump `position_hold_count` by 1 (saturating
/// at 7). Used by callers that already have an open `update_with(_at)`
/// closure (e.g., `propose_action`'s chain-stitch writes that combine
/// position-hold + slot-hold + force-position in one row). Those
/// callers must follow up with `propagate_position_hold_forward(ctx,
/// card_id, time_ms, +1)` so future rows pick up the same delta;
/// otherwise prefer the high-level `acquire_position_hold` helper.
pub fn increment_position_hold_count(flags: u32) -> u32 {
    let next = position_hold_count(flags)
        .saturating_add(1)
        .min(POSITION_HOLD_COUNT_MAX);
    write_position_hold_count(flags, next)
}

/// Pure flag transform: subtract 1 from `position_hold_count`
/// (saturating at 0). Mirror of [`increment_position_hold_count`] for
/// release paths; same "open closure" caveats and same need to follow
/// up with `propagate_position_hold_forward(..., -1)`.
pub fn decrement_position_hold_count(flags: u32) -> u32 {
    let next = position_hold_count(flags).saturating_sub(1);
    write_position_hold_count(flags, next)
}

/// Bookkeeping bits — server-managed `position_dirty` / `data_dirty`
/// plus the caller-set preserve markers, plus the
/// `position_hold_count` field. Excluded from the data-diff (so
/// toggling them doesn't itself count as a data change) and from
/// `propagate_flag_diff_forward`'s set/clear masks (so they don't
/// propagate themselves around).
///
/// Including `POSITION_HOLD_COUNT_MASK` here is what makes the count's
/// own dedicated forward-prop (`propagate_position_hold_forward`)
/// safe: the bit-by-bit propagator can't fight the count-aware
/// propagator over the same bits, because the bit-by-bit one ignores
/// the field entirely.
const BOOKKEEPING_FLAGS_MASK: u32 = FLAG_POSITION_DIRTY
    | FLAG_POSITION_PRESERVE
    | FLAG_DATA_DIRTY
    | FLAG_DATA_PRESERVE
    | POSITION_HOLD_COUNT_MASK;

/// Bit-mask of flags carried by `packed_definition`'s `CardDefinition.flags`,
/// or 0 when the definition isn't registered. Called from `create` /
/// `create_at` so any card spawned with a definition inherits its
/// flag set without per-call-site bookkeeping.
fn definition_flag_mask(packed: u16) -> u32 {
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
    pub macro_zone: u32,
    pub micro_zone: u8,
    pub micro_location: u32,
    #[index(btree)]
    pub owner_id: u32,
    pub packed_definition: u16,
    /// Bit-flag column. Currently u32 — wider than the populated bit
    /// range (bits 0..=7 today, defined in `content/cards/flags.json`).
    /// Will shrink to the smallest type that fits the final flag count
    /// once the registry stabilises.
    pub flags: u32,
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
/// exactly `time_ms`. Used by callers (e.g., `magnetic::end_magnetic_phase`)
/// that want to remove a previously-future-stamped row by its time
/// without knowing the opaque `sequence` portion of its PK. There's
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

    // Auto-set `position_dirty` / `data_dirty` by diffing the row
    // we're about to insert against the prior row. Strips any
    // caller-set value for those bits (they're server-managed) but
    // preserves the caller-set `position_preserve` / `data_preserve`
    // intent flags. With no prior row (fresh card_id), treat
    // everything as dirty so the very first row carries the markers
    // any future forward-prop will key off.
    card.flags &= !(FLAG_POSITION_DIRTY | FLAG_DATA_DIRTY);
    let (auto_pos, auto_data) = match prev_latest.as_ref() {
        Some(prev) => {
            let pos_changed = card.surface != prev.surface
                || card.macro_zone != prev.macro_zone
                || card.micro_zone != prev.micro_zone
                || card.micro_location != prev.micro_location;
            // Data diff excludes bookkeeping bits — toggling
            // position_dirty / data_dirty / *_preserve isn't itself
            // a state change.
            let data_changed = card.owner_id != prev.owner_id
                || card.packed_definition != prev.packed_definition
                || (card.flags & !BOOKKEEPING_FLAGS_MASK)
                    != (prev.flags & !BOOKKEEPING_FLAGS_MASK);
            (pos_changed, data_changed)
        }
        None => (true, true),
    };
    if auto_pos {
        card.flags |= FLAG_POSITION_DIRTY;
    }
    if auto_data {
        card.flags |= FLAG_DATA_DIRTY;
    }

    if ctx.db.cards().valid_at().find(card.valid_at).is_some() {
        ctx.db.cards().valid_at().delete(card.valid_at);
    }
    let inserted = ctx.db.cards().insert(card);
    crate::schedule_delete_cards::enqueue(ctx, inserted.card_id, inserted.valid_at);
    crate::souls::on_card_write(ctx, prev_latest.as_ref(), &inserted, time_ms);
    if let Some(prev) = prev_latest.as_ref() {
        propagate_flag_diff_forward(ctx, &inserted, prev.flags, time_ms);
    }
    inserted
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
    prev_flags: u32,
    time_ms: u64,
) {
    // Exclude only the bookkeeping bits from the diff — those are
    // server-managed per-row and shouldn't propagate themselves.
    let prev_user = prev_flags & !BOOKKEEPING_FLAGS_MASK;
    let new_user = new_card.flags & !BOOKKEEPING_FLAGS_MASK;
    let set_bits = new_user & !prev_user;
    let clear_bits = prev_user & !new_user;
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

    for v in future {
        if active_set == 0 && active_clear == 0 {
            break;
        }
        let Some(row) = ctx.db.cards().valid_at().find(v) else {
            continue;
        };
        let row_flags = row.flags;

        // `data_preserve` rows opt out of forward-prop entirely — the
        // caller deliberately stamped a state at this point in time
        // and the per-bit stop-on-change rule can't distinguish
        // "deliberate-same-as-prior" from "inherited," so we honor the
        // explicit intent. Skip this row, but keep walking — a
        // preserve marker means "don't touch THIS row," not "stop
        // propagation entirely."
        if row_flags & FLAG_DATA_PRESERVE != 0 {
            continue;
        }

        // Narrow per-bit:
        //  - For set_bits: keep bits where row has 0 (matches prev's
        //    0, so the row inherited from before our write). Drop
        //    bits where row has 1 (someone deliberately set it).
        //  - For clear_bits: keep bits where row has 1 (matches
        //    prev's 1, so inherited). Drop bits where row has 0
        //    (someone deliberately cleared it).
        active_set &= !row_flags;
        active_clear &= row_flags;
        if active_set == 0 && active_clear == 0 {
            break;
        }

        let new_flags = (row_flags & !active_clear) | active_set;
        ctx.db.cards().valid_at().delete(v);
        let mut updated = row;
        updated.flags = new_flags;
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
    new_macro_zone: u32,
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

    for v in future {
        let Some(row) = ctx.db.cards().valid_at().find(v) else {
            continue;
        };
        let row_flags = row.flags;

        if row_flags & FLAG_POSITION_PRESERVE != 0 {
            // Author-pinned position. This row and everything past
            // it is off-limits — chain stitch / magnetic pull
            // shouldn't be yanked mid-action.
            break;
        }

        let pos_only = (row_flags & FLAG_POSITION_DIRTY != 0)
            && (row_flags & FLAG_DATA_DIRTY == 0);
        if pos_only {
            ctx.db.cards().valid_at().delete(v);
            continue;
        }

        // Mixed or data-only row — re-home its position fields to
        // the new destination. Leave the data fields (flags / owner /
        // packed_def) alone so a future flag-change row still applies
        // its intended change at the correct moment.
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
            updated.flags |= FLAG_POSITION_DIRTY;
        }
        ctx.db.cards().insert(updated);
    }
}

// Insert a brand-new card. valid_at is computed; pass 0 will be overwritten.
//
// The card's `flags` column is `flags | definition_flag_mask(packed_definition)`
// — every card spawned with a definition inherits that definition's flag
// set (e.g. an event card with `["drop_locked", "surface_locked"]` lands
// already drop-locked / surface-locked). Callers that need to ADD dynamic
// bits at spawn time still pass them via `flags`; they get OR'd in.
#[allow(clippy::too_many_arguments)]
pub fn create(
    ctx: &ReducerContext,
    card_id: u32,
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,
    micro_location: u32,
    owner_id: u32,
    packed_definition: u16,
    flags: u32,
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
            flags: flags | definition_flag_mask(packed_definition),
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

/// Acquire one position-hold reference on a card at `time_ms`. Bumps
/// the count on the row current at that time AND walks every
/// future-stamped row of the same card, incrementing the count there
/// too — so a future-stamped release row written by an earlier action
/// (with count=0 baked in) correctly reflects "but someone else is
/// still holding" once this acquire lands. Without the forward walk,
/// the earlier release would prematurely transition the count to 0
/// at its `valid_at_time` and the held bit would flicker off while a
/// later action still needs it.
///
/// Symmetric with [`release_position_hold`]; SET callsites use this
/// (or, when bundled with other mutations in one row, the pure
/// transform [`increment_position_hold_count`] inside the closure
/// followed by `propagate_position_hold_forward(..., +1)` here).
pub fn acquire_position_hold(ctx: &ReducerContext, card_id: u32, time_ms: u64) {
    update_with_at(ctx, card_id, time_ms, |c| {
        c.flags = increment_position_hold_count(c.flags);
    });
    propagate_position_hold_forward(ctx, card_id, time_ms, true);
}

/// Release one position-hold reference on a card at `time_ms`. Mirror
/// of [`acquire_position_hold`] — decrements the count at `time_ms`
/// and on every future-stamped row of the same card.
pub fn release_position_hold(ctx: &ReducerContext, card_id: u32, time_ms: u64) {
    update_with_at(ctx, card_id, time_ms, |c| {
        c.flags = decrement_position_hold_count(c.flags);
    });
    propagate_position_hold_forward(ctx, card_id, time_ms, false);
}

/// Apply ±1 to the `position_hold_count` field on every row of this
/// card with `valid_at_time > time_ms`. Bypasses `write_at` (direct
/// `delete` / `insert`) so we don't recursively fire the souls hook,
/// re-enqueue schedule-deletes, or run the bit-by-bit forward-prop
/// (`POSITION_HOLD_COUNT_MASK` is in `BOOKKEEPING_FLAGS_MASK`, so
/// that propagator skips this field anyway). `valid_at` PKs are
/// preserved across delete/insert so existing `schedule_delete_cards`
/// rows still target the correct (replacement) row.
///
/// Callers that already mutated the row at `time_ms` themselves
/// (e.g., chain-stitch in `propose_action` ORs in
/// `increment_position_hold_count` alongside other flag bits) call
/// this to extend the same delta into the future-row chain. The pure
/// transforms `increment_position_hold_count` /
/// `decrement_position_hold_count` plus this helper give the same
/// effect as `acquire_position_hold` / `release_position_hold` but
/// fit inside a caller's existing closure.
pub fn propagate_position_hold_forward(
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
        let new_flags = if increment {
            increment_position_hold_count(row.flags)
        } else {
            decrement_position_hold_count(row.flags)
        };
        ctx.db.cards().valid_at().delete(v);
        let mut updated = row;
        updated.flags = new_flags;
        ctx.db.cards().insert(updated);
    }
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
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,
    micro_location: u32,
    owner_id: u32,
    packed_definition: u16,
    flags: u32,
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
            flags: flags | definition_flag_mask(packed_definition),
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
        // codebase (see `cards::write_at`, `players::write`); avoids
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

// ---- single-field setters ---------------------------------------------

pub fn set_surface(ctx: &ReducerContext, card_id: u32, surface: u8) -> Option<Card> {
    update_with(ctx, card_id, |c| c.surface = surface)
}

pub fn set_macro_zone(ctx: &ReducerContext, card_id: u32, macro_zone: u32) -> Option<Card> {
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
