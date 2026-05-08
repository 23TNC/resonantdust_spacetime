use spacetimedb::{table, ReducerContext, Table};

use crate::packed::{pack_valid_at, valid_at_time};

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
    pub owner_id: u32,
    pub packed_definition: u16,
    pub flags: u8,
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
fn write_at(ctx: &ReducerContext, mut card: Card, time_secs: u32) -> Card {
    card.valid_at = pack_valid_at(card.card_id, time_secs);
    if ctx.db.cards().valid_at().find(card.valid_at).is_some() {
        ctx.db.cards().valid_at().delete(card.valid_at);
    }
    let inserted = ctx.db.cards().insert(card);
    crate::schedule_delete_cards::enqueue(ctx, inserted.card_id, inserted.valid_at);
    inserted
}

// Insert a brand-new card. valid_at is computed; pass 0 will be overwritten.
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
    flags: u8,
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
            flags,
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
// at the action's scheduled completion time.
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
    flags: u8,
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
            flags,
        },
        time_secs,
    )
}

// Allocate a fresh card_id by scanning the cards history for `max+1`.
// O(N) over every version row, but the max is unaffected by per-id
// duplication so the answer is correct. Inserts within the current
// reducer are visible to subsequent calls — three creates in a loop
// produce three distinct ids.
pub fn next_card_id(ctx: &ReducerContext) -> u32 {
    ctx.db
        .cards()
        .iter()
        .map(|c| c.card_id)
        .max()
        .map_or(1, |m| m + 1)
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

pub fn set_flags(ctx: &ReducerContext, card_id: u32, flags: u8) -> Option<Card> {
    update_with(ctx, card_id, |c| c.flags = flags)
}
