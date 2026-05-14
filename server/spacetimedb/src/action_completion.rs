use std::collections::BTreeSet;

use resonantdust_content::definition_core::find_packed_by_key;
use resonantdust_content::recipe_core::{
    Entity, ProductOwner, ProductPlace, Reagent, RecipeDef,
};
use spacetimedb::{log, ReducerContext};

use crate::cards;
use crate::packed::{pack_valid_at, unpack_macro_zone};
use crate::recipe_eval::HasMatches;
use crate::world_gen;
use crate::zones;

/// Address of a synthetic hex — a "hex card" that doesn't actually exist
/// as a `Card` row but is derived from a Zone tile byte. Passed alongside
/// `hex == 0` into [`apply`] when the calling action targets a tile-as-
/// hex (the `propose_action` synthetic-hex path).
///
/// Carries everything `apply` needs to (1) resolve
/// `ProductOwner::Hex` to the zone's owner, (2) consume the hex by
/// reverting the tile byte to the underlying biome via
/// `world_gen::biome_for(global_q, global_r)`, and (3) write
/// `ProductPlace::Location` products into the same tile.
#[derive(Debug, Clone, Copy)]
pub struct HexLocation {
    pub zone_id: u32,
    /// Packed `(macro_q, macro_r)` of the zone — preserved here so the
    /// consumption path can compute the tile's *global* hex coords
    /// (zone macro × 8 + tile col/row) without re-reading the Zone
    /// row, which is needed to call `world_gen::biome_for`.
    pub macro_zone: u32,
    pub col: u8,
    pub row: u8,
    /// Snapshot of the zone's `owner_id` at the time the synthetic hex
    /// was derived. Used for `ProductOwner::Hex` resolution so the
    /// inheritance behaves like a real hex card's `owner_id`.
    pub owner_id: u32,
}

// Bit positions / fields from `cards/flags.json` (see content crate).
// Typed as u32 to match `Card.flags`.
const FLAG_DROP_HOLD: u32 = 1 << 3;
const FLAG_SLOT_HOLD: u32 = 1 << 5;
const FLAG_DEAD: u32 = 1 << 7;
/// Every non-counted `*_hold` bit, OR'd together. Used as the clearing
/// mask for recipe-completion releases — anything in this mask gets
/// cleared on the release row, on the policy "if you wanted a permanent
/// variant, you'd have set `*_locked` instead." `position_hold` is
/// intentionally absent — it's ref-counted via the `position_hold_count`
/// field, so cleanup paths decrement-and-maybe-clear via
/// `cards::release_position_hold` instead of mask-blast clearing it.
/// Append future single-owner hold bits here.
const HOLD_FLAGS_MASK: u32 = FLAG_DROP_HOLD | FLAG_SLOT_HOLD;
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

/// Grace period (milliseconds) between a card's death (its `dead`-bit
/// flip) and the follow-up `schedule_delete_cards` sweep that wipes the
/// card's remaining row. The dead row itself has `valid_at_time =
/// completion_ms`; the sweep deletes rows with `valid_at_time < cutoff`
/// (strict less-than), so we schedule it at `completion_ms + grace` to
/// catch the dead row too. The grace is what clients use to see the dead
/// bit and run a death animation before the row evaporates from the wire.
const DEAD_ROW_GRACE_MS: u64 = 10_000;

/// Apply a recipe's completion outcomes (reagent consumption, product
/// generation, hold release) directly into the `cards` table with all
/// writes stamped at `completion_ms`.
///
/// **No scheduled reducer is involved.** SpacetimeDB's `valid_at` pattern
/// already supports "this state becomes current at time T" via
/// future-stamped rows — the client's `ValidAtTable.promote(now)`
/// surfaces each future-stamped row once wall-clock reaches its
/// `valid_at_time`. So all of an action's outcomes can be written
/// synchronously inside `propose_action`'s single transaction:
///
/// - Reagent consumption: a row at `completion_ms` with `dead` set.
/// - Products: brand-new card rows at `completion_ms`.
/// - Hold release: a row at `completion_ms` with `position_hold` /
///   `slot_hold` cleared.
///
/// The owner-resolution falls back through Root → Actor → Hex → Action
/// per `ProductOwner`, mirroring the recipe_core docs. Card states
/// (owners) are read at *proposal time* — the action's outcome is
/// committed atomically; later changes to those cards' owners do not
/// retroactively affect already-resolved products.
///
/// One `schedule_delete_cards` row is enqueued per consumed card at
/// `completion_ms + DEAD_ROW_GRACE_SECS` so the dead row itself is
/// eventually swept. The implicit sweep enqueued by `update_with_at`
/// (firing at `completion_ms`) clears every prior version but
/// preserves the dead row; this explicit second sweep catches it.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    ctx: &ReducerContext,
    recipe_def: &RecipeDef,
    hex: u32,
    root: u32,
    slots: &[u32],
    completion_ms: u64,
    caller_player_id: u32,
    // `hex_location` is `Some` when the action targets a tile-as-hex
    // (`hex == 0` plus `propose_action` resolved a synthetic hex).
    // `None` everywhere else — real hex cards, OnCreate, magnetic.
    hex_location: Option<HexLocation>,
    // Card-ids resolved by `recipe_eval::resolve_has` for the
    // recipe's `has` + `reagents.has` predicates. Empty when the
    // recipe declares no has-predicates. The tail of each list
    // (length = `recipe_def.reagents.has.X.len()`) is consumed at
    // completion; the prefix is non-consumed (predicate-only).
    has_matches: HasMatches,
) -> Result<(), String> {
    // ---- Identify the actor ------------------------------------------
    //
    // The actor is the card whose completion row carries the action's
    // `progress_style` value — the one a client renders the progress
    // bar against, and the source for `ProductOwner::Actor` inheritance.
    //
    // Selection precedence: `slots[0]` (if any slots), else `root` (if
    // provided), else `hex` (if provided), else `0`. The slots-first
    // rule is the recipe-author-facing contract: actor is always
    // slots[0] when the recipe declares slots. `root` is only the
    // actor in the OnCreate shape, where slots is empty and root is
    // the new card itself ("OnCreate root == actor" convention). The
    // hex fallback covers degenerate on_create.magnetic shapes where
    // only the anchor is resolved.
    //
    // For stack recipes with both root and slots — e.g.
    // `root: A, slots: B+C` — actor is B (slots[0]), not A. Root in
    // stack recipes is just the chain anchor, not the actor.
    //
    // Resolved *before* owner sources so `ProductOwner::Actor` reads
    // the same id used everywhere else in this function.
    let actor_id: u32 = if !slots.is_empty() {
        slots[0]
    } else if root != 0 {
        root
    } else {
        hex
    };

    // ---- Resolve owner sources --------------------------------------
    let actor_owner = if actor_id != 0 {
        cards::latest(ctx, actor_id).map(|c| c.owner_id).unwrap_or(0)
    } else {
        0
    };
    let hex_owner = if hex != 0 {
        cards::latest(ctx, hex).map(|c| c.owner_id).unwrap_or(0)
    } else if let Some(loc) = &hex_location {
        // Synthetic hex — its "owner" is the Zone's `owner_id`,
        // snapshotted at proposal time. `0` for world zones means
        // `resolve_owner`'s fallback chain kicks in (→ caller).
        loc.owner_id
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
    //
    // `consumed` tracks Card-row deaths. Synthetic-hex consumption is
    // tracked separately in `consume_synthetic_hex` because the path
    // is different: a synthetic hex has no Card row, so consuming it
    // means clearing the zone tile byte (write 0), not flipping a
    // `dead` bit on a row that doesn't exist.
    let mut consumed: BTreeSet<u32> = BTreeSet::new();
    let mut consume_synthetic_hex = false;
    for &reagent in &recipe_def.reagents.slots {
        match reagent {
            Reagent::Root => {
                if root != 0 {
                    consumed.insert(root);
                }
            }
            Reagent::Hex => {
                if hex != 0 {
                    consumed.insert(hex);
                } else if hex_location.is_some() {
                    consume_synthetic_hex = true;
                }
            }
            // 1-indexed: Slot(1) is the actor at slots[0]. Slot(0) and
            // out-of-range indices silently skip.
            Reagent::Slot(n) => {
                let n = n as usize;
                if n >= 1 {
                    if let Some(&id) = slots.get(n - 1) {
                        consumed.insert(id);
                    }
                }
            }
        }
    }

    // `reagents.has.*` / `reagents.has_below.*` consumption: the tail
    // of each matches list (length = `recipe_def.reagents.has.X.len()`
    // / `recipe_def.reagents.has_below.X.len()`) is what gets killed
    // at completion. The head (length = `recipe_def.has.X.len()` /
    // `recipe_def.has_below.X.len()`) is non-consumed (predicate-only)
    // and stays alive — released via the loop further below.
    for &id in has_matches
        .above
        .root_consumed(recipe_def.reagents.has.root.len())
    {
        if id != 0 {
            consumed.insert(id);
        }
    }
    for &id in has_matches
        .above
        .actor_consumed(recipe_def.reagents.has.actor.len())
    {
        if id != 0 {
            consumed.insert(id);
        }
    }
    for &id in has_matches
        .below
        .root_consumed(recipe_def.reagents.has_below.root.len())
    {
        if id != 0 {
            consumed.insert(id);
        }
    }
    for &id in has_matches
        .below
        .actor_consumed(recipe_def.reagents.has_below.actor.len())
    {
        if id != 0 {
            consumed.insert(id);
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
        cards::update_with_at(ctx, id, completion_ms, |c| {
            c.flags |= FLAG_DEAD;
            // Clear non-counted holds + force_position + progress_style
            // alongside the dead-bit bump. The row is dying; the action
            // that claimed it is over, so `slot_hold` and friends
            // shouldn't linger. Same mask the release loop below uses
            // for non-consumed slots.
            c.flags &= !(HOLD_FLAGS_MASK | FLAG_FORCE_POSITION | PROGRESS_STYLE_MASK);
            // `position_hold` is ref-counted — decrement this action's
            // contribution inline; the matching forward-prop on future
            // rows happens via the ctx-aware call below.
            c.flags = cards::decrement_position_hold_count(c.flags);
            c.flags |= pack_progress_style(style);
        });
        // Forward-prop the -1 onto rows past `completion_ms` so any
        // later action's release/death row still reflects "but I'm
        // no longer here." Done after the inline mutation so the
        // closure's count change is already in the row at
        // completion_ms before we walk forward.
        cards::propagate_position_hold_forward(ctx, id, completion_ms, false);
        let cleanup_ms = completion_ms.saturating_add(DEAD_ROW_GRACE_MS);
        // The enqueue helper reads `valid_at_time(packed)` to derive its
        // own scheduling; we don't have (or need) the dead row's actual
        // PK here since the schedule keys off card_id + scheduled_at.
        // Just need a u64 whose `valid_at_time` decodes to `cleanup_ms`,
        // i.e., the time portion is correct. Sequence portion = 0 is
        // fine here (the schedule row doesn't go in the cards table).
        crate::schedule_delete_cards::enqueue(ctx, id, pack_valid_at(cleanup_ms, 0));
    }

    // Synthetic-hex consumption: revert the tile byte to its
    // *underlying biome* — what `world_gen::tile_for` would have
    // produced before the per-tile variant roll. Chopping a tree
    // leaves the forest tile it was placed on; mining a rock leaves
    // forest; consuming forest itself reverts to plains (the biome
    // under forest). Pure-deterministic re-derivation from the same
    // noise the generator used, so the revert is byte-equivalent to
    // what the world looked like before the tree existed.
    //
    // Explicit `location: { hex: [...] }` outputs in the recipe run
    // *after* this revert (in the product loop below), so a recipe
    // author who wants a specific tile (or an empty hex via a sentinel
    // tile if we ever add one) can override the auto-revert by
    // declaring a location output.
    //
    // `set_tile_at` future-stamps the Zone version row at
    // `completion_ms`, so the client's promote-by-time logic keeps
    // showing the original tile until the action actually completes —
    // matches the Card-death `update_with_at` discipline.
    if consume_synthetic_hex {
        if let Some(loc) = &hex_location {
            let (macro_q, macro_r) = unpack_macro_zone(loc.macro_zone);
            let global_q = macro_q as i32 * 8 + loc.col as i32;
            let global_r = macro_r as i32 * 8 + loc.row as i32;
            let underlying = world_gen::biome_for(global_q, global_r);
            zones::set_tile_at(
                ctx,
                loc.zone_id,
                completion_ms,
                loc.row,
                loc.col,
                underlying,
            );
        }
    }

    // ---- Generate products ------------------------------------------
    //
    // Products are new cards arriving at completion — they're outputs of
    // the actor's work, not actors themselves. They land Free with
    // `progress_style = 0`; their first row is its own existence, not a
    // progress event reference.
    //
    // Each product runs through `on_create::trigger` so OnCreate recipes
    // can match against it — useful when a recipe's output is a card
    // that itself triggers further automation (e.g. a magnetic anchor,
    // or an instant-effect card with `duration = 0`). Cascading is the
    // recipe author's responsibility to keep acyclic.
    for group in &recipe_def.output_success {
        match group.target.place {
            ProductPlace::Inventory => {
                let owner = resolve_owner(group.target.owner);
                for entity in &group.entities {
                    let packed_def = resolve_product_entity(entity)?;
                    let new_id = cards::next_card_id(ctx);
                    // Inventory placement convention (mirrors
                    // utilities::add_card): surface=1, macro_zone=owner,
                    // no spatial coords — layout is the client's job.
                    cards::create_at(
                        ctx,
                        new_id,
                        completion_ms,
                        /* surface         */ 1,
                        /* macro_zone      */ owner,
                        /* micro_zone      */ 0,
                        /* micro_location  */ 0,
                        /* owner_id        */ owner,
                        packed_def,
                        /* flags           */ 0,
                    );
                    // Pass `completion_ms` (the product's own
                    // `valid_at`) rather than `now`: the new card's
                    // only row is future-stamped, and
                    // `on_create::trigger`'s `cards::prior_at` call
                    // needs the matching upper bound to see it. With
                    // `now`, the fetch silently returns `None` and
                    // the OnCreate recipe is never installed — see
                    // `on_create::trigger`'s `time_secs` doc.
                    crate::on_create::trigger(ctx, new_id, caller_player_id, completion_ms)?;
                }
            }
            ProductPlace::Location => {
                // Only `ProductOwner::Hex` is implemented today (the
                // parser rejects other owners at parse time). The
                // tile address comes from the synthetic-hex
                // `hex_location`; if the action ran against a real
                // hex card or had no hex at all, there's no tile
                // address to write to — error so the recipe author
                // sees the misuse instead of silent drop.
                let Some(loc) = &hex_location else {
                    return Err(format!(
                        "recipe {}: location output requires a synthetic-hex action \
                         (hex must be omitted by the client; surface must carry tile data)",
                        recipe_def.id
                    ));
                };
                // Multi-entity groups: a tile byte can hold at most
                // one def_id. Per the agreed policy, take the first
                // entity and warn so the recipe author sees the
                // truncation without the action failing.
                if group.entities.len() > 1 {
                    log::warn!(
                        "recipe {}: location output group has {} entities; only the first will be written",
                        recipe_def.id,
                        group.entities.len()
                    );
                }
                let Some(entity) = group.entities.first() else {
                    continue;
                };
                let packed_def = resolve_product_entity(entity)?;
                // packed_definition: [card_type:u4 | card_category:u4 |
                // def_id:u8]. Tile bytes store only the def_id — low
                // 8 bits. The high byte (type+category) must match
                // the destination zone's `packed_definition`, but we
                // trust the recipe author's `Entity::Card("tree")`
                // alongside a `tile/default` zone to be consistent;
                // a future enhancement can verify equality.
                let def_id = (packed_def & 0xFF) as u8;
                zones::set_tile_at(ctx, loc.zone_id, completion_ms, loc.row, loc.col, def_id);
            }
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
    // Predicate-only (non-consumed) has-matches carry `position_hold`
    // (stamped by `resolve_has` so equipment can't be dragged off the
    // soul mid-action) but NOT `slot_hold` — so other concurrent
    // recipes can predicate-only-match them too. They survive the
    // action and need their `position_hold` released at
    // `completion_ms` via the loop below, so we add them to the
    // release set. Both directions (UP / DOWN) contribute. Consumed
    // has-matches are already in `consumed` and have their holds
    // cleared via the death-row mask.
    for &id in has_matches
        .above
        .root_predicate_only(recipe_def.reagents.has.root.len())
    {
        if id != 0 {
            release.insert(id);
        }
    }
    for &id in has_matches
        .above
        .actor_predicate_only(recipe_def.reagents.has.actor.len())
    {
        if id != 0 {
            release.insert(id);
        }
    }
    for &id in has_matches
        .below
        .root_predicate_only(recipe_def.reagents.has_below.root.len())
    {
        if id != 0 {
            release.insert(id);
        }
    }
    for &id in has_matches
        .below
        .actor_predicate_only(recipe_def.reagents.has_below.actor.len())
    {
        if id != 0 {
            release.insert(id);
        }
    }
    for &id in &consumed {
        release.remove(&id);
    }
    // Release clears every `*_hold` bit (per the project rule — if you
    // want permanence, use `*_locked` instead), plus `force_position`
    // and `progress_style`. Non-actors come out with style = 0; actor's
    // style is then re-set with a fresh value in the loop below.
    //
    // **Spatial fields are NOT touched on non-bottom released cards.**
    // The chain reshape after a consumed card dies is the client's job,
    // handled by `CardManager.spliceCard` once the dying card's death
    // animation has completed (gated on `dead === 2`). At that point
    // `transplantSlotChildren` promotes a state-1 child to inherit
    // the dying card's `micro_zone` / `micro_location` / `macro_zone`
    // / `surface` byte-for-byte — so a card freed from a world-tile
    // chain ends up state-3 OnHex on that tile, and a card freed from
    // an inventory chain inherits whatever inventory shape the dying
    // card had, all without the server having to second-guess client-
    // owned visual timing.
    //
    // Leaving non-bottom cards' spatial alone keeps the server row
    // pointing at the (about-to-be-deleted) parent until the dead-row
    // sweep at `completion_ms + DEAD_ROW_GRACE_SECS`. That's fine —
    // by then the client has already transplanted, and the mirror's
    // orphan-slot defensive recovery handles any reload edge-case
    // where the parent vanishes before the local view caught up.
    //
    // **EXCEPTION: chain bottom relocates to inventory when its
    // current `surface != 1`.** A chain action that completed on the
    // world (or any non-inventory surface — trade window, hypothetical
    // surface 32, etc.) sends its bottom card home. Every other
    // released card stays put — their `micro_location` parent pointers
    // make them follow the bottom automatically. The bottom of the
    // released chain is `root` if a root was provided, else `slots[0]`
    // if there are slots. The hex-only context (no root, no slots —
    // e.g. an on_create.magnetic anchor) has no chain bottom to
    // relocate (`bottom_id = 0`); hex tiles don't move regardless.
    //
    // The relocate writes `surface=1, macro_zone=c.owner_id,
    // micro_zone=0, micro_location=0` — i.e. loose-at-default-position
    // in the owner's inventory. Client lays out from there.
    let bottom_id: u32 = if root != 0 {
        root
    } else if !slots.is_empty() {
        slots[0]
    } else {
        0
    };
    let release_mask: u32 =
        !(HOLD_FLAGS_MASK | FLAG_FORCE_POSITION | PROGRESS_STYLE_MASK);

    for &id in &release {
        let is_actor = id == actor_id;
        let is_bottom = bottom_id != 0 && id == bottom_id;
        let style = if is_actor { actor_style } else { PROGRESS_STYLE_NONE };
        cards::update_with_at(ctx, id, completion_ms, |c| {
            c.flags = (c.flags & release_mask) | pack_progress_style(style);
            // `position_hold` is ref-counted — decrement this action's
            // contribution. The forward-prop afterward extends the
            // -1 onto any rows past `completion_ms` (release rows
            // from later actions that need to reflect the lower
            // count).
            c.flags = cards::decrement_position_hold_count(c.flags);
            if is_bottom && c.surface != 1 {
                c.surface = 1;
                c.macro_zone = c.owner_id;
                c.micro_zone = 0;
                c.micro_location = 0;
            }
        });
        cards::propagate_position_hold_forward(ctx, id, completion_ms, false);
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
