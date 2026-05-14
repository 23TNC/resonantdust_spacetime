use std::collections::BTreeSet;

use resonantdust_content::definition_core::{decode_definition, CardDefinition};
use resonantdust_content::recipe_core::{
    /* match_stack_recipe, */ recipe, Duration, Entity, RecipeType, StackDirection,
};
use spacetimedb::{reducer, ReducerContext};

use crate::action_completion;
use crate::cards;
use crate::packed::{
    self, pack_slot_micro_zone, pack_stack_micro_zone, unpack_micro_zone, valid_at_time,
    StackedState,
};
use crate::players;
use crate::recipe_eval::{aspect_pool, entity_satisfied_pool};
// Module + the generated `zones()` accessor trait — needed for the
// synthetic-hex lookup. Same `(self, … as _)` pattern as
// `magnetic.rs` / `world_gen.rs`.
use crate::zones::zones as _zones_table;
// use crate::stacks::CardStack;  // commented along with submit_action below

// `cards/flags.json` bit positions. Append-only — pinned by the data crate.
// Typed as u32 to match `Card.flags`.
const FLAG_SLOT_HOLD: u32 = 1 << 5;
/// Caller-set marker: "movement (and other position-prop) must not
/// touch this row's spatial fields." Stamped on chain-stitch / hex-pin
/// writes here so a queued move_soul can't yank a card mid-recipe.
const FLAG_POSITION_PRESERVE: u32 = 1 << 14;
// Surfaces ≥ this are world-layer; below is inventory-ish. Mirrored from
// `action_completion::WORLD_LAYER` so both modules agree on the boundary
// that distinguishes "rect sits on a hex tile in the world" from
// "rect is loose on an inventory surface".
const WORLD_LAYER: u8 = 64;
/// Surfaces ≥ this carry hex-tile data inside their backing `Zone` row.
/// Surfaces below this (player inventory panels, future personal
/// surfaces) have no hex tiles, so a `propose_action` that omits a
/// `hex` card on a low surface simply means "this action has no hex".
/// `32..64` is reserved for future pocket-dimension-style surfaces
/// that *do* carry tile data; world surfaces start at [`WORLD_LAYER`].
const SYNTHETIC_HEX_MIN_SURFACE: u8 = 32;

/// Resolve a synthetic hex for an action whose client passed `hex = 0`.
/// Returns the full `packed_definition` (so the recipe matcher sees
/// the same entity shape a real tile-Card would have) plus a
/// [`HexLocation`] (so [`action_completion::apply`] can write back
/// to the same tile for consumption / location outputs).
///
/// Reads the Zone at `(surface, macro_zone)`, extracts the tile byte
/// at the `(q, r)` decoded from `micro_zone`, and combines it with
/// the zone's `packed_definition` (which encodes the tile catalog's
/// type + category) to produce the full `packed_definition`.
///
/// Returns `None` when:
/// - `surface < SYNTHETIC_HEX_MIN_SURFACE` (panel surface — no zone
///   tile data here, no hex semantics).
/// - The `micro_zone` byte's state field isn't `OnHex` (client must
///   address a hex tile via that state to opt into this path).
/// - No Zone row exists at `(surface, macro_zone)` (unmapped area).
/// - The tile byte is `0` (no tile here — empty / cleared).
///
/// On `None`, the caller falls back to `hex_def = 0` and `hex_location
/// = None`. The matcher rejects the recipe if it declared `hex`;
/// otherwise the action proceeds as a chain-only proposal.
fn derive_synthetic_hex(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,
) -> Option<(u16, action_completion::HexLocation)> {
    if surface < SYNTHETIC_HEX_MIN_SURFACE {
        return None;
    }
    let (q, r, state) = unpack_micro_zone(micro_zone);
    if state != StackedState::OnHex {
        return None;
    }
    let zone = ctx
        .db
        .zones()
        .macro_zone()
        .filter(macro_zone)
        .filter(|z| z.surface == surface)
        .max_by_key(|z| valid_at_time(z.valid_at))?;
    let tile_byte = packed::tile_byte(zone.tile_row(r).unwrap_or(0), q as usize);
    if tile_byte == 0 {
        return None;
    }
    // `packed_definition` layout: `[card_type:u4 | card_category:u4 |
    // def_id:u8]`. The zone's `packed_definition` is `[type:u4 |
    // category:u4]` already packed into a u8 — shift it up by 8 to
    // open the low byte for the tile def_id.
    let packed_def = ((zone.packed_definition as u16) << 8) | (tile_byte as u16);
    let location = action_completion::HexLocation {
        zone_id: zone.zone_id,
        macro_zone: zone.macro_zone,
        col: q,
        row: r,
        owner_id: zone.owner_id,
    };
    Some((packed_def, location))
}
/// Mask for the `progress_style` 3-bit field at bits 8..=10. With-holds
/// rows clear this so any value inherited from a prior completion row
/// doesn't make this non-event row render a progress bar on the client.
const PROGRESS_STYLE_MASK: u32 = 0b111 << 8;
/// `force_position` (bit 11). Server is asserting this row's
/// `micro_zone` / `micro_location` verbatim — the client mirror should
/// overwrite local state and renumber conflicting entries. Replaces the
/// older `micro_zone.local_q = 1` disagree-with-anything trick (which
/// shared a field with real q-coordinate data). Set on with-holds rows
/// where the server has stitched a chain layout; explicitly cleared
/// otherwise so an inherited bit doesn't keep firing on later rows.
const FLAG_FORCE_POSITION: u32 = 1 << 11;

// ----- submit_action and its helpers — commented out 2026-05-09 -----
//
// Not currently needed. The active reducer is `propose_action` below
// (client-proposed recipe verification + flag/chain-stitch). Restore by
// removing the surrounding `/* */`, re-enabling the imports tagged
// `commented along with submit_action below`, and (per the earlier
// note) updating the per-card writeback to use `pack_stack_micro_zone`
// for stacked positions instead of `pack_micro_zone`.
/*
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
///    `micro_location`, every card above sits `OnCardTop` the one below.
/// 7. If the resolved `surface < 64` and the new `stacked_state != 0`,
///    sets `FLAG_FORCE_POSITION` on the row so the client's mirror
///    accepts the server's `micro_zone` / `micro_location` verbatim
///    (overwriting any local state and renumbering conflicting
///    entries). Replaces the older "set `micro_zone.local_q = 1`"
///    disagree-with-anything trick.
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
            (StackedState::OnRoot, chain[i - 1])
        };

        // Root pinned to hex overrides whatever chain position
        // suggested — the card is physically on the hex, not on
        // chain[i-1] (or free at a spatial position). State flips to
        // OnHex (= 3, same value as the client's `STACKED_ON_HEX`
        // in `pixijs/src/game/cards/cardData.ts`), and `micro_location`
        // becomes the hex card's id. Doing this *before* the q=1 force
        // rule means hex-pinned roots also pick up the resync trick.
        // Fixes the prior inconsistency where a root with no
        // `stack_down` ended up with `state=Free` but a
        // card-id-typed `micro_location`.
        if card_id == stack.root && root_pinned_to_hex {
            state = StackedState::OnHex;
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
            // With-holds rows aren't progress events — clear any
            // inherited progress_style so the client doesn't render
            // this row as one.
            c.flags &= !PROGRESS_STYLE_MASK;
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
*/
// ----- end of commented-out submit_action region -----

/// Verify a client-proposed recipe, stitch its slot cards into a chain,
/// and assign its hold flags.
///
/// Unlike [`submit_action`] (which searches for the best-matching recipe),
/// this reducer takes a specific `recipe_id` and verifies that the
/// supplied cards satisfy its entities. The server then writes the chain
/// under the (root_id, position) layout (see `packed.rs` header) so
/// every slot card's `micro_location` points at the chain root and its
/// row carries `FLAG_FORCE_POSITION` — the client mirror's
/// preserve rule respects the force bit and snaps the client view to
/// the server's chain.
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
/// **Chain layout written to slot cards:**
///
/// - The "chain root" is `root` when `root != 0`, otherwise `slots[0]`
///   (the actor) — a free-floating action makes the actor the loose
///   root of its own one-direction chain.
/// - Every slot that isn't the chain root gets:
///   - `micro_location = chain_root_id`
///   - `micro_zone = pack_stack_micro_zone(position, force_flag = false,
///     stacked_state)`, where position is the card's 1-indexed place in
///     the chain (actor = 1 when `root != 0`, slots[1] = 1 when
///     `root == 0`) and stacked_state is OnCardTop for `Stack(Up)` /
///     OnCardBottom for `Stack(Down)`.
/// - The chain root itself is not repositioned (its existing row is
///   left as the location-of-truth for the chain).
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
/// completion. All writes go through `cards::update_with` so each
/// stamps a fresh `valid_at` and enqueues the delete sweep.
#[reducer]
pub fn propose_action(
    ctx: &ReducerContext,
    hex: u32,
    root: u32,
    slots: Vec<u32>,
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,        // World-rooted only: tile's local (q, r) with
                           // state=OnHex, used when root == 0 && hex == 0
                           // && surface >= WORLD_LAYER. Ignored otherwise
                           // (rooted / hex-anchored anchors derive their
                           // micro_zone from the anchor card).
    micro_location: u32,   // legacy / unused; kept on the wire for binding
                           // stability.
    recipe_id: u16,
    root_dist: u8,         // actor's distance from root in the new layout.
                           // Used only when `root != 0`; the actor's
                           // `OnRoot` row gets `position = root_dist`.
                           // For a fresh chain with no held cards,
                           // typically `1`; for sub-roots past held
                           // blocks this is the full distance from the
                           // chain root.
) -> Result<(), String> {
    // `micro_zone` is consumed only by the world-rooted anchor case
    // below (root == 0, hex == 0, surface >= WORLD_LAYER) — it carries
    // the virtual hex tile's local (q, r) with state=OnHex. The
    // rooted and hex-anchored cases derive their micro_zone from the
    // chain's anchor card instead. `micro_location` is still legacy
    // on the wire; kept for binding stability.
    let _ = micro_location;
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

    // Resolve hex def. Two paths:
    //
    // 1. **Real hex card** (`hex != 0`): standard lookup against the
    //    `cards` table. Same shape this codebase has always had.
    // 2. **Synthetic hex from a zone tile** (`hex == 0`, surface carries
    //    tile data): derive the `packed_definition` from the Zone at
    //    `macro_zone` and the tile byte at the `(q, r)` decoded from
    //    `micro_zone`. Gives the matcher the same entity-shaped data
    //    a real Card would, without materializing a card row.
    //
    // If neither path resolves (no hex card, no tile under the cursor,
    // or panel-surface), `hex_def = 0` — the matcher will reject the
    // recipe if it declared `hex` and accept it otherwise.
    let (hex_def, hex_location) = if hex != 0 {
        let def = cards::latest(ctx, hex)
            .ok_or_else(|| format!("hex card {hex} not found"))?
            .packed_definition;
        (def, None)
    } else {
        match derive_synthetic_hex(ctx, surface, macro_zone, micro_zone) {
            Some((def, loc)) => (def, Some(loc)),
            None => (0, None),
        }
    };

    // Resolve root def + slot_hold guard only when a root was provided.
    // Without a root we have no anchor card to validate against.
    //
    // The full location-quartet check (surface, macro_zone, micro_zone,
    // micro_location) that lived here previously is gone: under the
    // new layout the client will rewrite root's `micro_zone` and
    // `micro_location` (e.g. when re-anchoring root to a hex via this
    // very reducer), so checking those fields against the client's
    // pre-write claim catches false-positive drift. We still validate
    // (surface, macro_zone) since those drive which subscriptions see
    // the row — a mismatch there would mean the client and server
    // disagree about which zone the recipe is happening in.
    let root_def = if root != 0 {
        let root_card = cards::latest(ctx, root)
            .ok_or_else(|| format!("root card {root} not found"))?;
        if root_card.flags & FLAG_SLOT_HOLD != 0 {
            return Err(format!(
                "root card {root} is already claimed by another in-flight action"
            ));
        }
        if root_card.surface != surface || root_card.macro_zone != macro_zone {
            return Err(format!(
                "root card {root} not in proposed zone \
                 (server: surface={}, macro_zone={}; proposed: surface={surface}, macro_zone={macro_zone})",
                root_card.surface,
                root_card.macro_zone,
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

    // Chain-depth bound: when this action pins the actor to root (i.e.
    // `root != 0`), `slot[0]` gets a state-2 (`OnRoot`) row whose
    // `position` field is `root_dist`. `pack_stack_micro_zone` masks
    // `position & 0x1f`, so any depth past 31 silently truncates and
    // corrupts chain layout. Mirrors the client's `DragManager` and
    // `ActionManager.tryMatch` rejection at the same threshold —
    // having both gates means a misbehaving client can't sneak a
    // chain-corrupting action past the server. Rootless actions
    // (`root == 0`) don't write state-2 here, so they don't hit this.
    if root != 0 {
        let pin_depth = root_dist as usize + slots.len();
        if pin_depth > 31 {
            return Err(format!(
                "recipe {recipe_id} would pin past chain index 31: \
                 root_dist={root_dist} + slots.len={} = {pin_depth}",
                slots.len()
            ));
        }
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

    // Resolve direction once from the recipe — Stack(Up) → STACK_DIR_UP,
    // Stack(Down) → STACK_DIR_DOWN. Used by both the slot chain
    // (state-1) and the actor-on-root pin (state-2) so they share a
    // direction. The recipe_type guard above already established this
    // is `Stack(_)`, so the unreachable arm only fires if that guard
    // is later relaxed without updating this.
    let direction = match recipe_def.recipe_type {
        RecipeType::Stack(StackDirection::Up) => crate::packed::STACK_DIR_UP,
        RecipeType::Stack(StackDirection::Down) => crate::packed::STACK_DIR_DOWN,
        _ => unreachable!("recipe_type guard above ensures Stack(_)"),
    };

    // Apply per-card writes per the lock-phase contract:
    //
    //   if Slots.length:
    //     slot_hold(Slot[0])
    //     for i in 1..N:
    //       Slot[i] → state=Slot, microLocation=Slot[i-1],
    //                 direction, macro_zone, surface,
    //                 slot_hold + position_hold + force_position.
    //
    //     Slot[0]'s spatial anchor depends on the chain shape (see
    //     `actor_anchor` above):
    //       - root != 0:
    //           Slot[0] → state=OnRoot, microLocation=root,
    //                     position=root_dist, direction,
    //                     macro_zone, surface,
    //                     position_hold + force_position.
    //       - root == 0 && hex != 0:
    //           Slot[0] → state=OnHex, microZone=hex's (q,r),
    //                     microLocation=hex card_id, macro_zone, surface
    //                     (all taken from hex card's row),
    //                     position_hold + force_position.
    //       - root == 0 && hex == 0 && surface >= WORLD_LAYER:
    //           Slot[0] → state=OnHex, microZone=caller's (carries q,r),
    //                     microLocation=0, macro_zone, surface
    //                     (all taken from caller args),
    //                     position_hold + force_position.
    //       - otherwise: no spatial change (rootless inventory action).
    //
    //   if root:
    //     slot_hold(root)
    //     if hex:
    //       root → state=OnHex, microZone=hex's (q,r),
    //              microLocation=hex card_id, macro_zone, surface,
    //              position_hold + force_position.
    //
    // `slot_hold` / `position_hold` flags are OR'd in. `force_position`
    // is set on every row that gets spatial fields rewritten so the
    // client's mirror takes the server's bytes verbatim. With-holds
    // rows clear `progress_style` so they don't render as completion
    // events on the client.
    //
    // Recipe-side `set_start` flags are applied per role at the same
    // time as the built-in holds: `set_start.slot` ORed onto every
    // slot card, `set_start.root` onto root (if present),
    // `set_start.hex` onto hex (if present). `set_start.slot` applies
    // uniformly across all slots in the matched recipe — per-index
    // overrides aren't a current need. Held flags clear at completion
    // via `action_completion::apply`'s `release_mask` (every `*_hold`
    // bit); permanent variants are the `*_locked` flags, which the
    // release_mask doesn't touch.
    // `set_start` is `FlagOps` per role — set/clear bitmasks built
    // from the JSON `"flag": true|false` entries. Each consumer site
    // below applies them *after* the server defaults so the author's
    // explicit `false` can release auto-held bits (e.g.,
    // `set_start.root.slot_hold = false` to drop the auto slot_hold
    // chain-stitching would otherwise leave on a slot).
    let set_start_root = recipe_def.set_start.root;
    let set_start_slot = recipe_def.set_start.slot;
    let set_start_hex = recipe_def.set_start.hex;
    let with_holds_flags_clear = !(PROGRESS_STYLE_MASK | FLAG_FORCE_POSITION);
    // Reducer-call timestamp, captured once. Used by both the chain-
    // stitch position-hold forward-propagation below and the
    // `completion_ms` calculation further down. `cards::now_ms` is
    // ctx-timestamp-based so multiple calls within the same reducer
    // return the same value; precomputing just makes the dataflow
    // explicit.
    let start_ms = crate::cards::now_ms(ctx);

    // Precompute slots[0]'s spatial anchor. Four cases:
    //   1. `root != 0`                        → state-2 OnRoot pin onto
    //                                            root at `root_dist`.
    //   2. `root == 0 && hex != 0`            → state-3 OnHex onto the hex
    //                                            card; (surface, macro_zone,
    //                                            q, r) come from hex's row.
    //   3. `root == 0 && hex == 0
    //       && surface >= WORLD_LAYER`        → state-3 OnHex onto a
    //                                            virtual world tile (no
    //                                            hex card row); the
    //                                            caller's (surface,
    //                                            macro_zone, micro_zone)
    //                                            encode the tile.
    //   4. otherwise                          → no spatial change; slots[0]
    //                                            keeps its current loose
    //                                            position (rootless
    //                                            inventory action).
    //
    // The server is authoritative on slot spatial state: the client's
    // local overlay for world drops is local-only, so without this
    // write the server would leave slots[0]'s stale (pre-drop) spatial
    // bytes intact and push them back, collapsing the chain on the
    // client mirror.
    let actor_anchor: Option<(u8, u32, u8, u32)> = if slots.is_empty() {
        None
    } else if root != 0 {
        Some((
            surface,
            macro_zone,
            pack_stack_micro_zone(root_dist, direction, StackedState::OnRoot),
            root,
        ))
    } else if hex != 0 {
        let hex_card = cards::latest(ctx, hex)
            .ok_or_else(|| format!("hex card {hex} not found"))?;
        if hex_card.surface != surface || hex_card.macro_zone != macro_zone {
            return Err(format!(
                "hex card {hex} not in proposed zone \
                 (server: surface={}, macro_zone={}; proposed: surface={surface}, macro_zone={macro_zone})",
                hex_card.surface, hex_card.macro_zone,
            ));
        }
        let (q, r, _) = unpack_micro_zone(hex_card.micro_zone);
        Some((
            hex_card.surface,
            hex_card.macro_zone,
            crate::packed::pack_micro_zone(q, r, StackedState::OnHex),
            hex,
        ))
    } else if surface >= WORLD_LAYER {
        // World-rooted: trust the caller's tile coords. The state bits
        // in `micro_zone` must say OnHex — anything else is a client
        // bug we don't want to silently paper over.
        let (_, _, state) = unpack_micro_zone(micro_zone);
        if state != StackedState::OnHex {
            return Err(format!(
                "world-rooted action requires micro_zone state=OnHex; got {state:?}"
            ));
        }
        Some((surface, macro_zone, micro_zone, 0))
    } else {
        None
    };

    if !slots.is_empty() {
        // slots[0] (actor): always gets slot_hold. Spatial fields when
        // `actor_anchor` is `Some` (the three anchored cases above) —
        // also bumps position_hold_count + force_position so the
        // client mirror accepts the server's bytes verbatim.
        let actor_takes_position_hold = actor_anchor.is_some();
        cards::update_with(ctx, slots[0], |c| {
            c.flags &= with_holds_flags_clear;
            c.flags |= FLAG_SLOT_HOLD;
            if let Some((s, mz_macro, mz_micro, ml)) = actor_anchor {
                c.surface = s;
                c.macro_zone = mz_macro;
                c.micro_zone = mz_micro;
                c.micro_location = ml;
                // `position_preserve` only when we actually pinned a
                // position. Rootless inventory actions (actor_anchor =
                // None) leave the card where the player put it, so a
                // later move_soul is free to re-home it.
                c.flags = cards::increment_position_hold_count(c.flags)
                    | FLAG_FORCE_POSITION
                    | FLAG_POSITION_PRESERVE;
            }
            // set_start.slot runs last so the author can override the
            // auto-held bits set above (slot_hold, position_hold).
            c.flags = set_start_slot.apply(c.flags);
        });
        if actor_takes_position_hold {
            // Forward-prop the count delta we just applied to any
            // future rows of this card (release rows queued by other
            // in-flight actions on the same card, etc.).
            cards::propagate_position_hold_forward(ctx, slots[0], start_ms, true);
        }

        // slots[1..]: state-1 (Slot) parent-pointer chain anchored on
        // slots[0]. Each slot's microLocation is its immediate
        // predecessor. Direction is stored explicitly. Always
        // `position_preserve` — these slots' positions are entirely
        // derived from the chain and movement must not reposition them.
        for i in 1..slots.len() {
            let parent_id = slots[i - 1];
            cards::update_with(ctx, slots[i], |c| {
                c.flags &= with_holds_flags_clear;
                c.flags = cards::increment_position_hold_count(c.flags)
                    | FLAG_SLOT_HOLD
                    | FLAG_FORCE_POSITION
                    | FLAG_POSITION_PRESERVE;
                c.flags = set_start_slot.apply(c.flags);
                c.micro_location = parent_id;
                c.micro_zone = pack_slot_micro_zone(direction);
                c.macro_zone = macro_zone;
                c.surface = surface;
            });
            cards::propagate_position_hold_forward(ctx, slots[i], start_ms, true);
        }
    }

    // Root: in stack recipes, root is the chain anchor — not the actor
    // (actor is slots[0]; see action_completion::apply's actor_id
    // resolution). Per the recipe-author contract, slot_hold goes on
    // the actor + slot fillers, not on root in stack recipes.
    // `set_start.root` bits and the hex-anchor spatial pin are still
    // applied here; the difference vs. earlier revisions is just that
    // we no longer OR in slot_hold.
    //
    // Spatial fields only when hex is provided (state-3 re-anchor
    // onto hex tile). When root is present without hex, root keeps
    // its existing position — the chain hangs off it where the player
    // put it.
    if root != 0 {
        // For the hex-anchor case, read the hex card's row to grab
        // its (localQ, localR) — those are the in-zone coords we
        // stamp on root's micro_zone so the client knows which hex
        // tile root sits on.
        let hex_qr: Option<(u8, u8)> = if hex != 0 {
            cards::latest(ctx, hex).map(|hex_card| {
                let (q, r, _) = unpack_micro_zone(hex_card.micro_zone);
                (q, r)
            })
        } else {
            None
        };
        let root_takes_position_hold = hex_qr.is_some();
        cards::update_with(ctx, root, |c| {
            c.flags &= with_holds_flags_clear;
            if let Some((q, r)) = hex_qr {
                c.micro_zone = crate::packed::pack_micro_zone(q, r, StackedState::OnHex);
                c.micro_location = hex;
                c.macro_zone = macro_zone;
                c.surface = surface;
                // Pinned onto the hex tile — `position_preserve` so
                // movement can't yank the root off the hex mid-action.
                c.flags = cards::increment_position_hold_count(c.flags)
                    | FLAG_FORCE_POSITION
                    | FLAG_POSITION_PRESERVE;
            }
            c.flags = set_start_root.apply(c.flags);
        });
        if root_takes_position_hold {
            cards::propagate_position_hold_forward(ctx, root, start_ms, true);
        }
    }

    // Hex: not otherwise touched by the lock-phase, but if the recipe's
    // `set_start.hex` carries any non-zero set/clear masks we apply
    // them to the hex card's row. Skipping the write when both
    // `hex == 0` and the ops are all-zero avoids a no-op version row.
    if hex != 0 && (set_start_hex.set_mask != 0 || set_start_hex.clear_mask != 0) {
        cards::update_with(ctx, hex, |c| {
            c.flags = set_start_hex.apply(c.flags);
        });
    }

    // Apply the completion outcomes synchronously, with all writes
    // stamped at start_secs + duration. The valid_at pattern handles
    // the "this becomes current later" behaviour automatically — the
    // client's ValidAtTable.promote(now) surfaces each future-stamped
    // row once wall-clock catches up. No scheduler involved.
    let duration_secs = match &recipe_def.duration {
        Some(Duration::Fixed(s)) => *s,
        Some(Duration::Conditional { cases, fallback }) => {
            // Pool: root (when present) + every slot's def. Hex tier
            // aspects are excluded by convention — duration is a
            // chain-side property, not an anchor property. Defs are
            // re-decoded here rather than threaded down from the
            // earlier eligibility loop; the registry lookups are
            // O(log n) and the chain is short.
            let mut chain_defs: Vec<&CardDefinition> = Vec::new();
            if root != 0 {
                if let Some(d) = decode_definition(root_def)
                    .map_err(|err| format!("recipe {recipe_id}: decode root def for pool: {err}"))?
                {
                    chain_defs.push(d);
                }
            }
            for (i, &packed) in slot_defs.iter().enumerate() {
                if let Some(d) = decode_definition(packed).map_err(|err| {
                    format!("recipe {recipe_id}: decode slot {i} def for pool: {err}")
                })? {
                    chain_defs.push(d);
                }
            }
            let pool = aspect_pool(chain_defs);
            // Cases evaluate in declaration order; first match wins.
            // Falls back to the trailing default when no case fires.
            let mut hit: Option<u32> = None;
            for (secs, entity) in cases {
                if entity_satisfied_pool(entity, &pool).map_err(|err| {
                    format!("recipe {recipe_id}: conditional duration entity: {err}")
                })? {
                    hit = Some(*secs);
                    break;
                }
            }
            hit.unwrap_or(*fallback)
        }
        None => return Err(format!("recipe {recipe_id}: missing duration")),
    };
    // `start_ms` was precomputed at the top of the lock-phase. Recipe
    // `duration` is authored in whole seconds (JSON int); convert to
    // ms for the time-budget arithmetic everywhere downstream.
    let completion_ms = start_ms.saturating_add((duration_secs as u64) * 1_000);

    // Capture the caller's player_id for ProductOwner::Action
    // resolution. Falling back to 0 if the caller has no session keeps
    // unauthenticated callers from breaking the reducer; products land
    // with owner_id=0 in that pathological case.
    let caller_player_id = players::resolve_caller(ctx).unwrap_or(0);

    // Resolve `has` / `reagents.has` predicates against the soul
    // stacks of the root's owner and the actor's owner. Empty
    // `HasMatches` when the recipe declares no has-predicates.
    //
    // Owner resolution mirrors `action_completion::apply`'s actor
    // pick: actor is `slots[0]` if slots exist, else root, else hex.
    // The owners of those cards are what `has.root` / `has.actor`
    // checks against — both are resolved from the cards table here.
    let root_owner = if root != 0 {
        cards::latest(ctx, root).map(|c| c.owner_id).unwrap_or(0)
    } else {
        0
    };
    let actor_id_for_has = if !slots.is_empty() {
        slots[0]
    } else if root != 0 {
        root
    } else {
        hex
    };
    let actor_owner = if actor_id_for_has != 0 {
        cards::latest(ctx, actor_id_for_has)
            .map(|c| c.owner_id)
            .unwrap_or(0)
    } else {
        0
    };
    let has_matches = crate::recipe_eval::resolve_has(
        ctx,
        &recipe_def.id,
        &recipe_def.has,
        &recipe_def.reagents.has,
        &recipe_def.has_below,
        &recipe_def.reagents.has_below,
        root_owner,
        actor_owner,
        start_ms,
    )?;

    action_completion::apply(
        ctx,
        recipe_def,
        hex,
        root,
        &slots,
        completion_ms,
        caller_player_id,
        hex_location,
        has_matches,
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

// ----- apply_match_holds — commented out alongside submit_action -----
/*
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
*/
