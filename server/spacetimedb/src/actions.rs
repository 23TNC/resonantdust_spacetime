use std::collections::{BTreeMap, BTreeSet};

use resonantdust_content::definition_core::{decode_definition, CardDefinition};
use resonantdust_content::recipe_core::{
    match_stack_recipe, recipe, Duration, Entity, RecipeType, StackDirection,
};
use spacetimedb::{reducer, ReducerContext};

use crate::action_completion;
use crate::cards;
use crate::packed::{pack_micro_zone, unpack_micro_zone, StackedState};
use crate::players;
use crate::stacks::CardStack;

// `cards/flags.json` bit positions. Append-only — pinned by the data crate.
const FLAG_POSITION_HOLD: u8 = 1 << 0;
const FLAG_SLOT_HOLD: u8 = 1 << 5;

/// Submit one or more card stacks to the action system.
///
/// For each stack, the server:
/// 1. Validates structural well-formedness (cards exist, no duplicates).
/// 2. Runs `Stack(Up)` and `Stack(Down)` recipe matching independently
///    against the chain. Either, both, or neither may match.
/// 3. For every matched recipe: sets `slot_hold` on every card filling a
///    slot, and `position_hold` on every slot **except the first** (the
///    actor — see below).
/// 4. Sets `position_hold` on the actor of any matched direction iff
///    `root != 0` or `hex != 0` (locks the actor to its current stack).
/// 5. Sets `position_hold` on `stack.root` iff `hex != 0` (locks the
///    root to the hex card) and overwrites `root.micro_location = hex`.
/// 6. Rewrites every card's `micro_location` (and `micro_zone` state
///    bits) to reflect the chain — bottom card is `Free` at the stack's
///    `micro_location`, every card above sits `OnCard` the one below.
/// 7. If the resolved `surface < 64` and the new `stacked_state != 0`,
///    forces `micro_zone.local_q = 1`. This deliberately disagrees with
///    most plausible client-side q values so the next sync round-trip
///    snaps the client back into line with the server.
///
/// `position_hold` and `slot_hold` are **not** touched on cards outside
/// matched recipes. Cards already carrying those flags from a previous
/// action keep them; the release path is the recipe-completion logic
/// (not yet implemented).
///
/// All writes go through `cards::update_with`, so each card's
/// `valid_at` is stamped to "now" and a one-shot delete schedule is
/// enqueued. Recipe execution proper — reagent consumption, product
/// generation, hold release — is intentionally deferred and will be
/// scheduled with future `valid_at`s once the recipe-runner lands.
///
/// **Sentinel-value caveats.** Per the agreed reducer signature, `0`
/// means "not provided" for `root`, `hex`, `surface`, and `macro_zone`.
/// `card_id` 0 is reserved so that's safe for the first two; `surface=0`
/// (world layer 0) and `macro_zone=0` (origin tile) are legitimate
/// values that this signature can't express as overrides — pass non-zero
/// or use the original `cards::set_*` helpers if you need to write a
/// genuine zero. `micro_zone` carries no override semantics here; it's
/// taken as-is and re-packed per chain position.
#[reducer]
pub fn submit_action(
    ctx: &ReducerContext,
    stacks: Vec<CardStack>,
    root: u32,
    hex: u32,
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,
) -> Result<(), String> {
    for (i, stack) in stacks.iter().enumerate() {
        validate_stack(ctx, stack).map_err(|e| format!("stacks[{i}]: {e}"))?;
    }

    for (i, stack) in stacks.iter().enumerate() {
        process_stack(ctx, stack, root, hex, surface, macro_zone, micro_zone)
            .map_err(|e| format!("stacks[{i}]: {e}"))?;
    }

    Ok(())
}

// Cards exist + no duplicates across root + stack_up + stack_down.
fn validate_stack(ctx: &ReducerContext, stack: &CardStack) -> Result<(), String> {
    let mut chain = Vec::with_capacity(1 + stack.stack_up.len() + stack.stack_down.len());
    chain.push(stack.root);
    chain.extend(stack.stack_up.iter().copied());
    chain.extend(stack.stack_down.iter().copied());

    let mut seen = BTreeSet::new();
    for &id in &chain {
        if !seen.insert(id) {
            return Err(format!("card {id} appears more than once"));
        }
        if cards::latest(ctx, id).is_none() {
            return Err(format!("card {id} does not exist"));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_stack(
    ctx: &ReducerContext,
    stack: &CardStack,
    arg_root: u32,
    arg_hex: u32,
    arg_surface: u8,
    arg_macro_zone: u32,
    arg_micro_zone: u8,
) -> Result<(), String> {
    // Resolve packed_definitions for the recipe matcher.
    let hex_def = if arg_hex != 0 {
        cards::latest(ctx, arg_hex)
            .ok_or_else(|| format!("hex card {arg_hex} not found"))?
            .packed_definition
    } else {
        0
    };
    let root_card = cards::latest(ctx, stack.root)
        .ok_or_else(|| format!("root card {} not found", stack.root))?;
    let root_def = root_card.packed_definition;

    let up_defs = pack_chain_defs(ctx, &stack.stack_up);
    let down_defs = pack_chain_defs(ctx, &stack.stack_down);

    let up_match = match_stack_recipe(hex_def, root_def, &up_defs, StackDirection::Up)
        .map_err(|e| format!("recipe match (up): {e}"))?;
    let down_match = match_stack_recipe(hex_def, root_def, &down_defs, StackDirection::Down)
        .map_err(|e| format!("recipe match (down): {e}"))?;

    // Holds for cards in matched recipes.
    let mut slot_holds: BTreeSet<u32> = BTreeSet::new();
    let mut position_holds: BTreeSet<u32> = BTreeSet::new();

    apply_match_holds(up_match, &stack.stack_up, &mut slot_holds, &mut position_holds)?;
    apply_match_holds(down_match, &stack.stack_down, &mut slot_holds, &mut position_holds)?;

    // Actor pinning: if the action carries either a root or a hex context,
    // the actor of any matched direction is locked to its current stack.
    let any_match = up_match != 0 || down_match != 0;
    if any_match && (arg_root != 0 || arg_hex != 0) {
        if up_match != 0 {
            if let Some(&actor) = stack.stack_up.first() {
                position_holds.insert(actor);
            }
        }
        if down_match != 0 {
            if let Some(&actor) = stack.stack_down.first() {
                position_holds.insert(actor);
            }
        }
    }

    // Root pinned to hex.
    let root_pinned_to_hex = arg_hex != 0;
    if root_pinned_to_hex {
        position_holds.insert(stack.root);
    }

    // Resolve the new (surface, macro_zone) for the root. 0 = absent =>
    // keep what the root currently has. micro_zone has no override
    // sentinel; we always re-pack it per chain position below.
    let new_surface = if arg_surface != 0 { arg_surface } else { root_card.surface };
    let new_macro_zone = if arg_macro_zone != 0 { arg_macro_zone } else { root_card.macro_zone };
    let (q_arg, r_arg, _) = unpack_micro_zone(arg_micro_zone);

    // Bottom-to-top chain: stack_down reversed → root → stack_up.
    let mut chain: Vec<u32> = Vec::with_capacity(
        1 + stack.stack_up.len() + stack.stack_down.len(),
    );
    chain.extend(stack.stack_down.iter().rev().copied());
    chain.push(stack.root);
    chain.extend(stack.stack_up.iter().copied());

    // Apply per-card writes. Each goes through cards::update_with so
    // valid_at = now and the schedule_delete_cards sweep is enqueued.
    for (i, &card_id) in chain.iter().enumerate() {
        let (mut state, mut micro_location) = if i == 0 {
            (StackedState::Free, stack.micro_location)
        } else {
            (StackedState::OnCard, chain[i - 1])
        };

        // Root pinned to hex overrides whatever chain position
        // suggested — the card is physically on the hex, not on
        // chain[i-1] (or free at a spatial position). State flips to
        // STACKED_ON_HEX (server's `Reserved3` = client's
        // `STACKED_ON_HEX` = 3 in `pixijs/src/game/cards/cardData.ts`),
        // and `micro_location` becomes the hex card's id. Doing this
        // *before* the q=1 force rule means hex-pinned roots also
        // pick up the resync trick. Fixes the prior inconsistency where
        // a root with no `stack_down` ended up with `state=Free` but a
        // card-id-typed `micro_location`.
        if card_id == stack.root && root_pinned_to_hex {
            state = StackedState::Reserved3;
            micro_location = arg_hex;
        }

        // surface<64 + non-Free => force q=1 so the next sync from the
        // client picks up the server's intent even if the client thinks
        // the card was somewhere else.
        let local_q = if new_surface < 64 && state != StackedState::Free {
            1
        } else {
            q_arg
        };
        let new_micro_zone = pack_micro_zone(local_q, r_arg, state);

        let set_pos = position_holds.contains(&card_id);
        let set_slot = slot_holds.contains(&card_id);

        cards::update_with(ctx, card_id, |c| {
            c.surface = new_surface;
            c.macro_zone = new_macro_zone;
            c.micro_zone = new_micro_zone;
            c.micro_location = micro_location;
            if set_pos {
                c.flags |= FLAG_POSITION_HOLD;
            }
            if set_slot {
                c.flags |= FLAG_SLOT_HOLD;
            }
        });
    }

    Ok(())
}

// Read packed_definitions for a chain of card_ids. Validation has already
// confirmed every id exists, so a `None` here would only arise from a race
// the SpacetimeDB reducer-serialization model rules out — we substitute 0
// in that case rather than propagate, since match_stack_recipe treats 0
// as "no card here" and will simply fail to match.
fn pack_chain_defs(ctx: &ReducerContext, ids: &[u32]) -> Vec<u16> {
    ids.iter()
        .map(|&id| cards::latest(ctx, id).map(|c| c.packed_definition).unwrap_or(0))
        .collect()
}

/// Verify a client-proposed recipe and assign its hold flags.
///
/// Unlike [`submit_action`] (which searches for the best-matching recipe),
/// this reducer takes a specific `recipe_id` and verifies that the
/// supplied cards satisfy its entities. No position rewriting happens —
/// the client is asserting "these cards are already in this recipe shape
/// at this location"; the server's job is to check + flag, not to move.
///
/// **Sentinel-zero semantics:** both `hex` and `root` use `0` to mean
/// "not provided". A `0` inside `slots` is always a client bug and is
/// rejected. The recipe itself decides whether `hex` / `root` are
/// required — if `recipe.hex.is_some()` and the caller passed `hex == 0`,
/// the request is rejected (and same for `root`).
///
/// **Validation steps (any failure rolls the whole reducer back):**
///
/// 1. No card id appears twice across `hex` / `root` / `slots`. Every
///    non-zero referenced card must exist.
/// 2. If `root != 0`, root's current row must match the proposed
///    `(surface, macro_zone, micro_zone, micro_location)` exactly —
///    catches client-server sync drift. If `root == 0`, the location
///    args are accepted but not enforced; there's no anchor card to
///    check them against.
/// 3. No card already carries `slot_hold` from another in-flight
///    action. Per `cards/flags.json`, a `slot_hold`-marked card is
///    "committed to the in-flight recipe and must not be re-claimed
///    by another match" — we enforce that contract here so concurrent
///    proposals against the same cards serialize as "first wins".
///    Checked on `root` and every `slots[i]`.
/// 4. `recipe_id` must be a registered `Stack(_)` recipe.
/// 5. Recipe eligibility: `recipe.hex` (if present) requires `hex != 0`
///    and the hex card's def must satisfy it; same for `recipe.root`.
///    Every `recipe.slots[i]` must be satisfied by `slots[i]`.
///    `slots.len()` must equal `recipe.slots.len()` exactly — extras
///    or shortfalls reject.
///
/// **Flag assignment** (mirrors `submit_action`):
///
/// - `slot_hold` set on every slot card.
/// - `position_hold` set on `slots[1..]` (the actor at `slots[0]` is
///   excluded from this rule).
/// - `position_hold` set on the actor iff `root != 0 || hex != 0` —
///   without either, the action is free-floating and the actor doesn't
///   need a positional pin.
/// - `position_hold` set on `root` iff `root != 0 && hex != 0`.
///
/// Flags are OR'd in, never cleared. Release happens at recipe
/// completion (not yet implemented). All writes go through
/// `cards::update_with` so each stamps a fresh `valid_at` and enqueues
/// the delete sweep.
#[reducer]
pub fn propose_action(
    ctx: &ReducerContext,
    hex: u32,
    root: u32,
    slots: Vec<u32>,
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,
    micro_location: u32,
    recipe_id: u16,
) -> Result<(), String> {
    // De-dup across hex / root / slots. Zero is the sentinel for "absent"
    // on hex and root (skipped from the dedup set); zero inside slots is
    // a client bug.
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    if root != 0 {
        seen.insert(root);
    }
    if hex != 0 && !seen.insert(hex) {
        return Err(format!("hex {hex} duplicates root"));
    }
    for &id in &slots {
        if id == 0 {
            return Err("slot card_id must be non-zero".to_string());
        }
        if !seen.insert(id) {
            return Err(format!("card {id} appears more than once"));
        }
    }

    // Resolve hex def.
    let hex_def = if hex != 0 {
        cards::latest(ctx, hex)
            .ok_or_else(|| format!("hex card {hex} not found"))?
            .packed_definition
    } else {
        0
    };

    // Resolve root def + enforce location and slot_hold guard only if a
    // root was provided. Without a root we have no anchor card to
    // validate the location args against; the args are accepted as
    // declarative context but unenforced.
    let root_def = if root != 0 {
        let root_card = cards::latest(ctx, root)
            .ok_or_else(|| format!("root card {root} not found"))?;
        if root_card.flags & FLAG_SLOT_HOLD != 0 {
            return Err(format!(
                "root card {root} is already claimed by another in-flight action"
            ));
        }
        if root_card.surface != surface
            || root_card.macro_zone != macro_zone
            || root_card.micro_zone != micro_zone
            || root_card.micro_location != micro_location
        {
            return Err(format!(
                "root card {root} not at proposed location \
                 (server: surface={}, macro_zone={}, micro_zone={}, micro_location={})",
                root_card.surface,
                root_card.macro_zone,
                root_card.micro_zone,
                root_card.micro_location,
            ));
        }
        root_card.packed_definition
    } else {
        0
    };

    let mut slot_defs: Vec<u16> = Vec::with_capacity(slots.len());
    for &id in &slots {
        let card = cards::latest(ctx, id)
            .ok_or_else(|| format!("slot card {id} not found"))?;
        if card.flags & FLAG_SLOT_HOLD != 0 {
            return Err(format!(
                "slot card {id} is already claimed by another in-flight action"
            ));
        }
        slot_defs.push(card.packed_definition);
    }

    // Verify the proposed recipe.
    let recipe_def = recipe(recipe_id)
        .map_err(|e| format!("recipe lookup: {e}"))?
        .ok_or_else(|| format!("recipe {recipe_id} not registered"))?;
    if !matches!(recipe_def.recipe_type, RecipeType::Stack(_)) {
        return Err(format!(
            "recipe {recipe_id} is not a stack recipe ({:?})",
            recipe_def.recipe_type
        ));
    }

    // Eligibility: every entity in the recipe must be satisfied by the
    // corresponding card. Mirrors the eligibility half of
    // `match_stack_recipe`, minus the specificity scoring (we don't
    // care about ranking when the client has named the recipe).
    if let Some(e) = &recipe_def.hex {
        let def = decode_definition(hex_def)
            .map_err(|err| format!("decode hex def: {err}"))?
            .ok_or_else(|| {
                format!("recipe {recipe_id} requires a hex card; none provided or unknown def")
            })?;
        if !entity_satisfied(e, def) {
            return Err(format!("recipe {recipe_id}: hex card does not satisfy hex entity"));
        }
    }
    if let Some(e) = &recipe_def.root {
        if root == 0 {
            return Err(format!("recipe {recipe_id} requires a root card; none provided"));
        }
        let def = decode_definition(root_def)
            .map_err(|err| format!("decode root def: {err}"))?
            .ok_or_else(|| format!("root card {root} has unknown definition"))?;
        if !entity_satisfied(e, def) {
            return Err(format!("recipe {recipe_id}: root card does not satisfy root entity"));
        }
    }
    if slots.len() != recipe_def.slots.len() {
        return Err(format!(
            "recipe {recipe_id} expects {} slot(s), got {}",
            recipe_def.slots.len(),
            slots.len()
        ));
    }
    for (i, slot_entity) in recipe_def.slots.iter().enumerate() {
        let def = decode_definition(slot_defs[i])
            .map_err(|err| format!("decode slot {i} def: {err}"))?
            .ok_or_else(|| format!("slot {i} card {} has unknown definition", slots[i]))?;
        if !entity_satisfied(slot_entity, def) {
            return Err(format!(
                "recipe {recipe_id}: slot {i} (card {}) does not satisfy slot entity",
                slots[i]
            ));
        }
    }

    // Assign flags.
    let mut slot_holds: BTreeSet<u32> = BTreeSet::new();
    let mut position_holds: BTreeSet<u32> = BTreeSet::new();

    for (i, &id) in slots.iter().enumerate() {
        slot_holds.insert(id);
        if i > 0 {
            position_holds.insert(id);
        }
    }
    // Actor pin: only meaningful when this action is anchored to a root
    // or a hex. A free-floating action (no root, no hex) leaves the
    // actor moveable — it's just slots in space.
    if root != 0 || hex != 0 {
        if let Some(&actor) = slots.first() {
            position_holds.insert(actor);
        }
    }
    // Root pinned to hex requires both to be present.
    if root != 0 && hex != 0 {
        position_holds.insert(root);
    }

    // Build a slot → "card below" map for chain stitching. Every slot
    // above the actor sits on the slot below it (`slots[i-1]`). The
    // actor (`slots[0]`) sits on root if there is one; with no root the
    // actor is the bottom and we leave its position untouched.
    let mut slot_below: BTreeMap<u32, u32> = BTreeMap::new();
    for (i, &id) in slots.iter().enumerate() {
        let below = if i == 0 {
            if root == 0 {
                continue;
            }
            root
        } else {
            slots[i - 1]
        };
        slot_below.insert(id, below);
    }
    // Slot cards get their micro_zone re-packed with `local_q = 1` and a
    // direction-appropriate `stacked_state`. The `q = 1` is the
    // deliberate disagree-with-anything trick — even if the client
    // thinks the card is somewhere else, the next sync round-trip snaps
    // it back into chain.
    //
    // Stack direction → state mapping matches the client convention in
    // `pixijs/src/game/cards/cardData.ts`:
    //
    // - `Stack(Up)`   → `OnCard`    = state 1 (`STACKED_ON_RECT_X`,
    //   client interprets as "I sit on top of micro_location").
    // - `Stack(Down)` → `Reserved2` = state 2 (`STACKED_ON_RECT_Y`,
    //   client interprets as "I sit on the bottom of micro_location").
    //
    // The server-side enum calling state 2 "Reserved2" is a misnomer —
    // the client gives it real meaning; rename it on the content side
    // when convenient. The recipe_type guard above already established
    // that this branch is `Stack(_)`, so the unreachable arm only fires
    // if that guard is later relaxed without updating this.
    let stacked_state = match recipe_def.recipe_type {
        RecipeType::Stack(StackDirection::Up) => StackedState::OnCard,
        RecipeType::Stack(StackDirection::Down) => StackedState::Reserved2,
        _ => unreachable!("recipe_type guard above ensures Stack(_)"),
    };
    let stacked_micro_zone = pack_micro_zone(1, 0, stacked_state);

    // Apply. Iterate the union of cards that are about to receive any
    // flag or chain-position update. Slot cards listed in `slot_below`
    // also get `micro_zone` + `micro_location` set; others (root, when
    // it has holds) only get flags. Skips cards we don't care about,
    // and naturally handles "no root provided" (root is never in any
    // of these sets then).
    let mut targets: BTreeSet<u32> = slot_holds.union(&position_holds).copied().collect();
    for &id in slot_below.keys() {
        targets.insert(id);
    }
    for id in targets {
        let set_pos = position_holds.contains(&id);
        let set_slot = slot_holds.contains(&id);
        let below = slot_below.get(&id).copied();
        cards::update_with(ctx, id, |c| {
            if set_pos {
                c.flags |= FLAG_POSITION_HOLD;
            }
            if set_slot {
                c.flags |= FLAG_SLOT_HOLD;
            }
            if let Some(below_id) = below {
                c.micro_zone = stacked_micro_zone;
                c.micro_location = below_id;
            }
        });
    }

    // Apply the completion outcomes synchronously, with all writes
    // stamped at start_secs + duration. The valid_at pattern handles
    // the "this becomes current later" behaviour automatically — the
    // client's ValidAtTable.promote(now) surfaces each future-stamped
    // row once wall-clock catches up. No scheduler involved.
    let duration_secs = match &recipe_def.duration {
        Some(Duration::Fixed(s)) => *s,
        Some(Duration::Conditional { .. }) => {
            return Err(format!(
                "recipe {recipe_id}: conditional durations not yet supported"
            ));
        }
        None => return Err(format!("recipe {recipe_id}: missing duration")),
    };
    let start_secs = (ctx.timestamp.to_micros_since_unix_epoch() / 1_000_000) as u32;
    let completion_secs = start_secs.saturating_add(duration_secs);

    // Capture the caller's player_id for ProductOwner::Action
    // resolution. Falling back to 0 if the caller has no session keeps
    // unauthenticated callers from breaking the reducer; products land
    // with owner_id=0 in that pathological case.
    let caller_player_id = players::resolve_caller(ctx).unwrap_or(0);

    action_completion::apply(
        ctx,
        recipe_def,
        hex,
        root,
        &slots,
        completion_secs,
        caller_player_id,
    )?;

    Ok(())
}

// Boolean version of recipe_core's private `entity_specificity`. Returns
// true iff the entity is satisfied by `def`. We don't need the
// specificity score here — `propose_action` only cares about
// eligibility, never about ranking against other recipes.
fn entity_satisfied(entity: &Entity, def: &CardDefinition) -> bool {
    match entity {
        Entity::Card(key) => &def.key == key,
        Entity::Aspect(aspect, min) => {
            let val = def
                .aspects
                .iter()
                .find_map(|(a, v)| (a == aspect).then_some(*v))
                .unwrap_or(0);
            val >= *min
        }
        Entity::Type(type_id) => def.card_type == *type_id,
        Entity::Any => true,
        Entity::And(a, b) => entity_satisfied(a, def) && entity_satisfied(b, def),
        Entity::Or(a, b) | Entity::WeightedOr { a, b, .. } => {
            entity_satisfied(a, def) || entity_satisfied(b, def)
        }
    }
}

// For a matched recipe id, mark the cards filling its slots:
// - all slot fillers get slot_hold;
// - everyone except slot[0] (the actor) gets position_hold from this rule.
//   The actor's own pin is governed separately by the root/hex args.
fn apply_match_holds(
    match_id: u16,
    chain: &[u32],
    slot_holds: &mut BTreeSet<u32>,
    position_holds: &mut BTreeSet<u32>,
) -> Result<(), String> {
    if match_id == 0 {
        return Ok(());
    }
    let recipe_def = recipe(match_id)
        .map_err(|e| format!("recipe lookup: {e}"))?
        .ok_or_else(|| format!("recipe {match_id} not registered"))?;
    let n = recipe_def.slots.len().min(chain.len());
    for (i, &card_id) in chain.iter().take(n).enumerate() {
        slot_holds.insert(card_id);
        if i > 0 {
            position_holds.insert(card_id);
        }
    }
    Ok(())
}
