use resonantdust_content::definition_core::decode_definition;
use spacetimedb::{table, ReducerContext, Table};

use crate::packed::{pack_valid_at, valid_at_time};

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

fn now_secs(ctx: &ReducerContext) -> u32 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000_000) as u32
}

// Latest row for a card_id is the row with the largest time component of valid_at.
pub fn latest(ctx: &ReducerContext, card_id: u32) -> Option<Card> {
    ctx.db
        .cards()
        .card_id()
        .filter(card_id)
        .max_by_key(|c| valid_at_time(c.valid_at))
}

// Stamp valid_at = (card_id, now) and write. If a row already exists at that
// exact key (two writes in the same second), the existing one is replaced —
// "always accept the most recent write". Also enqueues a one-shot delete
// schedule that will sweep older rows for this card_id once the scheduler
// fires.
fn write(ctx: &ReducerContext, card: Card) -> Card {
    write_at(ctx, card, now_secs(ctx))
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
fn write_at(ctx: &ReducerContext, mut card: Card, time_secs: u32) -> Card {
    card.valid_at = pack_valid_at(card.card_id, time_secs);
    let prev_latest = latest(ctx, card.card_id);
    if ctx.db.cards().valid_at().find(card.valid_at).is_some() {
        ctx.db.cards().valid_at().delete(card.valid_at);
    }
    let inserted = ctx.db.cards().insert(card);
    crate::schedule_delete_cards::enqueue(ctx, inserted.card_id, inserted.valid_at);
    crate::souls::on_card_write(ctx, prev_latest.as_ref(), &inserted, time_secs);
    if let Some(prev) = prev_latest.as_ref() {
        propagate_flag_diff_forward(ctx, &inserted, prev.flags, time_secs);
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
    time_secs: u32,
) {
    let set_bits = new_card.flags & !prev_flags;
    let clear_bits = prev_flags & !new_card.flags;
    if set_bits == 0 && clear_bits == 0 {
        return;
    }

    let mut future: Vec<u64> = ctx
        .db
        .cards()
        .card_id()
        .filter(new_card.card_id)
        .filter(|c| valid_at_time(c.valid_at) > time_secs)
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

// Pick up the latest row for `card_id`, mutate it via `f`, write it back.
// Returns None if no prior row exists.
pub fn update_with<F>(ctx: &ReducerContext, card_id: u32, f: F) -> Option<Card>
where
    F: FnOnce(&mut Card),
{
    let mut c = latest(ctx, card_id)?;
    f(&mut c);
    Some(write(ctx, c))
}

// Like `update_with`, but stamps the resulting row at a specific
// `time_secs` rather than `now`. Used by the action-completion path.
pub fn update_with_at<F>(
    ctx: &ReducerContext,
    card_id: u32,
    time_secs: u32,
    f: F,
) -> Option<Card>
where
    F: FnOnce(&mut Card),
{
    let mut c = latest(ctx, card_id)?;
    f(&mut c);
    Some(write_at(ctx, c, time_secs))
}

// Like `create`, but stamps the new row at a specific `time_secs` rather
// than `now`. Used by the action-completion path to materialize products
// at the action's scheduled completion time. Same definition-flag merge
// as `create`.
#[allow(clippy::too_many_arguments)]
pub fn create_at(
    ctx: &ReducerContext,
    card_id: u32,
    time_secs: u32,
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
        time_secs,
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
