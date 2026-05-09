use std::collections::BTreeSet;

use resonantdust_content::definition_core::find_packed_by_key;
use resonantdust_content::recipe_core::{Entity, ProductOwner, Reagent, RecipeDef};
use spacetimedb::ReducerContext;

use crate::cards;
use crate::packed::pack_valid_at;

// Bit positions / fields from `cards/flags.json` (see content crate).
// Typed as u32 to match `Card.flags`.
const FLAG_POSITION_HOLD: u32 = 1 << 0;
const FLAG_SLOT_HOLD: u32 = 1 << 5;
const FLAG_DEAD: u32 = 1 << 7;
/// `force_position` (bit 11). Set on every row `propose_action`
/// repositions; cleared here at completion. Once the recipe finishes,
/// the server has no view of where surviving cards belong (the chain
/// is broken, the client decides absolute positions), so leaving the
/// flag set would make the client mirror re-bump siblings on every
/// subsequent push of the released / consumed row.
const FLAG_FORCE_POSITION: u32 = 1 << 11;

/// `progress_style` (bits 8..=10). 3-bit field encoding the progress
/// bar style for this row's "next future event" client render. The
/// client reads `(flags >> 8) & 0b111` and renders accordingly.
///
/// Values:
///
/// - `0` = no progress (no bar shown)
/// - `1` = ltr / cw default
/// - `2` = rtl / ccw default
/// - `3..=7` reserved for future styles
///
/// Set on the actor's completion row by `action_completion::apply`;
/// explicitly cleared on with-holds rows in `propose_action` /
/// `submit_action` so non-event rows don't render bars from inherited
/// state.
const PROGRESS_STYLE_SHIFT: u32 = 8;
const PROGRESS_STYLE_MASK: u32 = 0b111 << PROGRESS_STYLE_SHIFT;
pub const PROGRESS_STYLE_NONE: u32 = 0;
pub const PROGRESS_STYLE_LTR: u32 = 1;
#[allow(dead_code)]
pub const PROGRESS_STYLE_RTL: u32 = 2;

/// Encode a `progress_style` value into the bit positions occupied by
/// the field. Caller is responsible for clearing `PROGRESS_STYLE_MASK`
/// from the destination first if the prior row may have a different
/// value set.
fn pack_progress_style(value: u32) -> u32 {
    (value & 0b111) << PROGRESS_STYLE_SHIFT
}

/// Grace period (seconds) between a card's death (its `dead`-bit flip)
/// and the follow-up `schedule_delete_cards` sweep that wipes the card's
/// remaining row. The dead row itself has `valid_at_time =
/// completion_secs`; the sweep deletes rows with `valid_at_time < cutoff`
/// (strict less-than), so we schedule it at `completion_secs + grace` to
/// catch the dead row too. The grace is what clients use to see the dead
/// bit and run a death animation before the row evaporates from the wire.
const DEAD_ROW_GRACE_SECS: u32 = 10;

/// Apply a recipe's completion outcomes (reagent consumption, product
/// generation, hold release) directly into the `cards` table with all
/// writes stamped at `completion_secs`.
///
/// **No scheduled reducer is involved.** SpacetimeDB's `valid_at` pattern
/// already supports "this state becomes current at time T" via
/// future-stamped rows — the client's `ValidAtTable.promote(now)`
/// surfaces each future-stamped row once wall-clock reaches its
/// `valid_at_time`. So all of an action's outcomes can be written
/// synchronously inside `propose_action`'s single transaction:
///
/// - Reagent consumption: a row at `completion_secs` with `dead` set.
/// - Products: brand-new card rows at `completion_secs`.
/// - Hold release: a row at `completion_secs` with `position_hold` /
///   `slot_hold` cleared.
///
/// The owner-resolution falls back through Root → Actor → Hex → Action
/// per `ProductOwner`, mirroring the recipe_core docs. Card states
/// (owners) are read at *proposal time* — the action's outcome is
/// committed atomically; later changes to those cards' owners do not
/// retroactively affect already-resolved products.
///
/// One `schedule_delete_cards` row is enqueued per consumed card at
/// `completion_secs + DEAD_ROW_GRACE_SECS` so the dead row itself is
/// eventually swept. The implicit sweep enqueued by `update_with_at`
/// (firing at `completion_secs`) clears every prior version but
/// preserves the dead row; this explicit second sweep catches it.
pub fn apply(
    ctx: &ReducerContext,
    recipe_def: &RecipeDef,
    hex: u32,
    root: u32,
    slots: &[u32],
    completion_secs: u32,
    caller_player_id: u32,
) -> Result<(), String> {
    // ---- Resolve owner sources --------------------------------------
    let actor_id = slots.first().copied().unwrap_or(0);
    let actor_owner = if actor_id != 0 {
        cards::latest(ctx, actor_id).map(|c| c.owner_id).unwrap_or(0)
    } else {
        0
    };
    let hex_owner = if hex != 0 {
        cards::latest(ctx, hex).map(|c| c.owner_id).unwrap_or(0)
    } else {
        0
    };
    let root_owner = if root != 0 {
        cards::latest(ctx, root).map(|c| c.owner_id).unwrap_or(0)
    } else {
        0
    };
    let resolve_owner = |o: ProductOwner| -> u32 {
        let candidate = match o {
            ProductOwner::Root => root_owner,
            ProductOwner::Actor => actor_owner,
            ProductOwner::Hex => hex_owner,
            ProductOwner::Action => caller_player_id,
        };
        if candidate != 0 {
            candidate
        } else {
            caller_player_id
        }
    };

    // ---- Identify the actor ------------------------------------------
    //
    // The actor is the card whose row carries `FLAG_DISPLAY_PROGRESS` at
    // completion — the one a client renders the progress bar against.
    // Convention: root if provided, else `slots[0]`. Hex isn't yet a
    // valid actor designation. `0` means no actor (zero-slot, no-root
    // recipe — degenerate case where no card shows a progress bar).
    let actor_id: u32 = if root != 0 {
        root
    } else {
        slots.first().copied().unwrap_or(0)
    };

    // ---- Identify consumed cards (reagents) --------------------------
    let mut consumed: BTreeSet<u32> = BTreeSet::new();
    for reagent in &recipe_def.reagents {
        match reagent {
            Reagent::Root => {
                if root != 0 {
                    consumed.insert(root);
                }
            }
            Reagent::Hex => {
                if hex != 0 {
                    consumed.insert(hex);
                }
            }
            // 1-indexed: Slot(1) is the actor at slots[0]. Slot(0) and
            // out-of-range indices silently skip.
            Reagent::Slot(n) => {
                let n = *n as usize;
                if n >= 1 {
                    if let Some(&id) = slots.get(n - 1) {
                        consumed.insert(id);
                    }
                }
            }
        }
    }

    // The progress style for this completion. Hard-coded to LTR until
    // the recipe-side change exposes a `progress_style: u8` field on
    // `RecipeDef`; once that lands, swap this for `recipe_def.progress_style`
    // (or whatever the parsed name ends up).
    let actor_style: u32 = PROGRESS_STYLE_LTR;

    // ---- Apply reagent consumption + cleanup-sweep enqueue ----------
    //
    // The dead-bit always goes on consumed cards. `progress_style` *only*
    // gets a non-zero value on the actor's completion row — a non-actor
    // reagent dies quietly without triggering a progress-bar render on
    // its own card. (If the actor itself is the reagent, both fields
    // land here.) We always clear the field first in case the prior
    // row had a stale value carried forward.
    for &id in &consumed {
        let is_actor = id == actor_id;
        let style = if is_actor { actor_style } else { PROGRESS_STYLE_NONE };
        cards::update_with_at(ctx, id, completion_secs, |c| {
            c.flags |= FLAG_DEAD;
            // Clear force_position alongside the progress-style bump.
            // The row is dying; once the client has applied the dead
            // flag and the death animation runs, position-forcing on
            // a doomed row only causes spurious sibling renumbers.
            c.flags &= !(PROGRESS_STYLE_MASK | FLAG_FORCE_POSITION);
            c.flags |= pack_progress_style(style);
        });
        let cleanup_secs = completion_secs.saturating_add(DEAD_ROW_GRACE_SECS);
        crate::schedule_delete_cards::enqueue(ctx, id, pack_valid_at(id, cleanup_secs));
    }

    // ---- Generate products ------------------------------------------
    //
    // Products are new cards arriving at completion — they're outputs of
    // the actor's work, not actors themselves. They land Free with
    // `progress_style = 0`; their first row is its own existence, not a
    // progress event reference.
    for group in &recipe_def.output_success {
        let owner = resolve_owner(group.target.owner);
        for entity in &group.entities {
            let packed_def = resolve_product_entity(entity)?;
            let new_id = cards::next_card_id(ctx);
            // Inventory placement convention (mirrors utilities::add_card):
            // surface=1 (inventory), macro_zone=owner, no spatial coords,
            // micro_location=0 — layout is the client's job.
            cards::create_at(
                ctx,
                new_id,
                completion_secs,
                /* surface         */ 1,
                /* macro_zone      */ owner,
                /* micro_zone      */ 0,
                /* micro_location  */ 0,
                /* owner_id        */ owner,
                packed_def,
                /* flags           */ 0,
            );
        }
    }

    // ---- Release flags on involved-but-not-consumed cards -----------
    //
    // Slot cards get un-chained: `micro_zone` resets to 0 (q=0, r=0,
    // state=Free) and `micro_location` to 0. The chain that
    // `propose_action` stitched together is broken at completion;
    // surviving slots become loose cards again with no `local_q = 1`
    // disagreement bit set. Root keeps its position — it was the
    // anchor, was never repositioned by the proposal, and stays where
    // the action found it.
    let mut release: BTreeSet<u32> = BTreeSet::new();
    if root != 0 {
        release.insert(root);
    }
    for &id in slots {
        release.insert(id);
    }
    for &id in &consumed {
        release.remove(&id);
    }
    // Release clears `position_hold`, `slot_hold`, `force_position`,
    // *and* `progress_style` (so non-actors come out with style = 0);
    // actor's style is then re-set with a fresh value below.
    // `force_position` matters: at completion the chain is dissolved
    // (`micro_zone` / `micro_location` reset to 0 below), so there's
    // nothing for the row to "force" — the client owns absolute
    // positions for surviving cards. Leaving the bit set would make
    // every subsequent server push of this row trigger a sibling
    // renumber on the client mirror.
    let release_mask: u32 =
        !(FLAG_POSITION_HOLD | FLAG_SLOT_HOLD | FLAG_FORCE_POSITION | PROGRESS_STYLE_MASK);
    for &id in &release {
        let is_root = root != 0 && id == root;
        let is_actor = id == actor_id;
        let style = if is_actor { actor_style } else { PROGRESS_STYLE_NONE };
        cards::update_with_at(ctx, id, completion_secs, |c| {
            c.flags = (c.flags & release_mask) | pack_progress_style(style);
            if !is_root {
                c.micro_zone = 0;
                c.micro_location = 0;
            }
        });
    }

    Ok(())
}

// Resolve a product entity to a single packed_definition.
//
// Recipe entities used in slot matching are general (Type/Aspect/And/Or
// pattern trees). Product entities, in practice, name a specific card to
// produce — `Entity::Card("key")` — possibly wrapped in a `WeightedOr`
// or `Or` to pick between alternatives. This function only resolves
// those variants; matcher-only shapes (Any/Type/Aspect/And) error out
// so a recipe authoring mistake fails loudly rather than silently
// producing nothing.
//
// **Branch picking is currently deterministic-by-first-branch.**
// `WeightedOr` and `Or` both pick branch `a`. Reducers in SpacetimeDB
// are deterministic by design and we don't have an RNG seed source
// threaded through. Once gameplay actually relies on weighted output
// variance, the obvious fix is hashing some action-identifying value
// into a uniform pick.
fn resolve_product_entity(entity: &Entity) -> Result<u16, String> {
    match entity {
        Entity::Card(key) => find_packed_by_key(key)
            .map_err(|e| format!("definition lookup: {e}"))?
            .ok_or_else(|| format!("product card key {key:?} not registered")),
        Entity::WeightedOr { a, .. } => resolve_product_entity(a),
        Entity::Or(a, _) => resolve_product_entity(a),
        Entity::Any | Entity::Type(_) | Entity::Aspect(_, _) | Entity::And(_, _) => Err(format!(
            "product entity must be a Card or weighted alternation; \
             matcher entities (Any/Type/Aspect/And) cannot resolve to a \
             single output card"
        )),
    }
}
