use std::collections::BTreeSet;

use resonantdust_content::definition_core::find_packed_by_key;
use resonantdust_content::recipe_core::{Entity, ProductOwner, Reagent, RecipeDef};
use spacetimedb::ReducerContext;

use crate::cards;
use crate::packed::pack_valid_at;

// Bit positions from `cards/flags.json` (see content crate).
const FLAG_POSITION_HOLD: u8 = 1 << 0;
const FLAG_SLOT_HOLD: u8 = 1 << 5;
const FLAG_DEAD: u8 = 1 << 7;

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

    // ---- Apply reagent consumption + cleanup-sweep enqueue ----------
    for &id in &consumed {
        cards::update_with_at(ctx, id, completion_secs, |c| {
            c.flags |= FLAG_DEAD;
        });
        let cleanup_secs = completion_secs.saturating_add(DEAD_ROW_GRACE_SECS);
        crate::schedule_delete_cards::enqueue(ctx, id, pack_valid_at(id, cleanup_secs));
    }

    // ---- Generate products ------------------------------------------
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
    let release_mask: u8 = !(FLAG_POSITION_HOLD | FLAG_SLOT_HOLD);
    for &id in &release {
        let is_root = root != 0 && id == root;
        cards::update_with_at(ctx, id, completion_secs, |c| {
            c.flags &= release_mask;
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
