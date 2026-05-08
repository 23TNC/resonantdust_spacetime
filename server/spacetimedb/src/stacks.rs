use spacetimedb::{ReducerContext, SpacetimeType};
use std::collections::BTreeSet;

use crate::cards;
use crate::packed::{pack_micro_zone, unpack_micro_zone, StackedState};

/// Wire-format description of a card stack as the client sees it. Not a
/// table — it's a reducer-argument struct that an action reducer accepts and
/// then applies to the `cards` table via [`apply`].
///
/// Fields:
/// - `root` — anchor card_id of the stack. Conceptually "the focal card";
///   doesn't have to be the topmost or bottommost.
/// - `surface` / `macro_zone` — shared by every card in the stack.
/// - `micro_zone` — only the q/r component is meaningful here. The
///   `stacked_state` bits are ignored; `apply` re-packs them per card
///   (Free for the bottom card, OnCard for everyone above it).
/// - `micro_location` — spatial position of the **bottom** card (interpreted
///   as packed `(x, y)` pixel coords when stack_down is empty and the root
///   itself is the bottom; otherwise it's the position of `stack_down.last()`).
/// - `stack_up` — cards stacked above the root, ordered from closest-to-root
///   (index 0, sits directly on root) to topmost (last index).
/// - `stack_down` — cards stacked below the root, ordered from closest-to-root
///   (index 0, root sits directly on it) to bottommost (last index).
#[derive(SpacetimeType, Debug, Clone)]
pub struct CardStack {
    pub root: u32,
    pub surface: u8,
    pub macro_zone: u32,
    pub micro_zone: u8,
    pub micro_location: u32,
    pub stack_up: Vec<u32>,
    pub stack_down: Vec<u32>,
}

/// Apply a stack's positioning info to the cards table.
///
/// Walks the bottom-to-top chain (`stack_down` reversed → `root` → `stack_up`)
/// and updates each card via `cards::update_with`:
///
/// - Every card gets the stack's `surface` and `macro_zone`.
/// - Every card gets `micro_zone` re-packed with the stack's q/r and a
///   per-card stacked_state — `Free` for the bottom card, `OnCard` for each
///   card above it.
/// - The bottom card gets `micro_location = stack.micro_location` (the
///   spatial position). Every other card gets `micro_location = (card_id of
///   the card directly below it)`, encoding the OnCard relationship.
///
/// Returns `Err` if the stack is empty, if a card_id appears more than once,
/// or if any card_id doesn't exist in the cards table — none of those are
/// valid client input.
pub fn apply(ctx: &ReducerContext, stack: &CardStack) -> Result<(), String> {
    // Reuse the stack's q/r; ignore whatever state bits the client sent.
    let (q, r, _) = unpack_micro_zone(stack.micro_zone);
    let micro_zone_free = pack_micro_zone(q, r, StackedState::Free);
    let micro_zone_on = pack_micro_zone(q, r, StackedState::OnCard);

    // Bottom-to-top: stack_down reversed, then root, then stack_up.
    let mut chain: Vec<u32> =
        Vec::with_capacity(1 + stack.stack_up.len() + stack.stack_down.len());
    chain.extend(stack.stack_down.iter().rev().copied());
    chain.push(stack.root);
    chain.extend(stack.stack_up.iter().copied());

    if chain.is_empty() {
        return Err("CardStack must contain at least the root".to_string());
    }

    let mut seen: BTreeSet<u32> = BTreeSet::new();
    for &c in &chain {
        if !seen.insert(c) {
            return Err(format!("card {c} appears more than once in stack"));
        }
    }

    // Bottom card sits Free at the stack's spatial position.
    let bottom = chain[0];
    cards::update_with(ctx, bottom, |c| {
        c.surface = stack.surface;
        c.macro_zone = stack.macro_zone;
        c.micro_zone = micro_zone_free;
        c.micro_location = stack.micro_location;
    })
    .ok_or_else(|| format!("card {bottom} (stack bottom) not found"))?;

    // Every other card sits OnCard the one below it. micro_location holds the
    // below card's id directly — `pack_micro_location_card_id` is identity,
    // so we just write the id.
    for window in chain.windows(2) {
        let below = window[0];
        let above = window[1];
        cards::update_with(ctx, above, |c| {
            c.surface = stack.surface;
            c.macro_zone = stack.macro_zone;
            c.micro_zone = micro_zone_on;
            c.micro_location = below;
        })
        .ok_or_else(|| format!("card {above} not found"))?;
    }

    Ok(())
}
