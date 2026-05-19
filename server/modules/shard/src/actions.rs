//! `propose_action` reducer — verifier for the tape-form recipe model.
//!
//! Wire format (per-iterator bindings — no inventory walks):
//!
//! ```text
//! propose_action(
//!   recipe_id: u16,
//!   surface: u8, macro_zone: u32, micro_zone: u8,  // root's intended location
//!   root: u32,
//!   bindings: Vec<Vec<u32>>,    // bindings[iter_id][offset] = card_id
//! )
//! ```
//!
//! The client pre-resolves every `slot.<branch>.<index>` reference and
//! sends the resulting card_ids per iterator. Top-level iterators
//! (`parent == []`) carry the player's action stack — those cards get
//! chain-stitched into branch `iterator.branch` at completion.
//! Nested iterators (`parent != []`) carry cards that already live in
//! some other chain (equipment, queued actions, …) — those are
//! predicate-verified but not chain-stitched.
//!
//! Branch 0 is the tile branch by convention. If a recipe references
//! branch 0 and `bindings[iter_for_branch_0][0] == 0` (the no-card
//! sentinel), the server synthesizes from zone tile data at
//! `(surface, macro_zone, micro_zone)`.
//!
//! Stages (any failure rolls the whole reducer back):
//!
//! 1. **Recipe-vs-stack** (fail-fast, in-memory): walk `recipe.input`,
//!    resolve each `Seg::Slot { iterator_id, offset }` via
//!    `bindings[iter_id][offset]`, evaluate predicate. Transition
//!    checks (`.owner` / `.parent` consistency, branch-direction
//!    constraint on nested iterators) ride alongside — all O(1)
//!    per segment, no scans.
//! 2. **Stack-vs-world** (DB-heavy): every card_id in bindings
//!    exists, no duplicates, no `slot_hold` from another action,
//!    owners chain back to caller, magnetic-flag discipline holds.
//! 3. **Chain-stitch**: write per-card position bytes for cards in
//!    top-level iterators' bindings, organized by `iterator.branch`.
//!    Nested-iterator bindings are left alone (they're in their own
//!    chains already).
//! 4. **Locks + schedule**: `slot_hold` on cards inside iterator
//!    windows (those consumed at completion); `position_hold` on
//!    every chain card to preserve structure; stamp the action row;
//!    schedule [`action_completion::apply`].

use std::collections::BTreeSet;

use resonantdust_content::definition_core::{
    aspect_id, decode_definition, is_aspect_descendant, lifecycle_recipe_for_def, AspectId,
};
use resonantdust_content::recipe_core::{recipe, Recipe, Seg};
use resonantdust_content::recipe_statement::StatementValue;
use spacetimedb::{reducer, ReducerContext};

use crate::action_completion::{self, HexLocation};
use crate::cards;
use crate::packed::{
    micro_zone_direction, pack_micro_zone, pack_slot_micro_zone, pack_stack_micro_zone,
    unpack_micro_zone, StackedState,
};
use crate::players;
// Module + the generated `zones()` accessor trait — needed for the
// synthetic-tile lookup. Same `(self, … as _)` pattern as elsewhere.
use crate::zones::zones as _zones_table;

// `cards/flags.json` bit positions. Append-only — pinned by the data
// crate. `#[allow(dead_code)]` until Stage 4 (locks/schedule) lands.
#[allow(dead_code)]
const FLAG_DEAD: u32 = 1 << 7;
#[allow(dead_code)]
const FLAG_SLOT_HOLD: u32 = 1 << 5;
#[allow(dead_code)]
const FLAG_POSITION_PRESERVE: u32 = 1 << 14;
#[allow(dead_code)]
const FLAG_FORCE_POSITION: u32 = 1 << 11;
#[allow(dead_code)]
const FLAG_LIFECYCLE_PENDING: u32 = 1 << 12;

/// Surfaces ≥ this carry hex-tile data inside their backing `Zone` row.
const SYNTHETIC_HEX_MIN_SURFACE: u8 = 32;

/// Tile card_type — see `movement.rs::TILE_CARD_TYPE`.
const TILE_CARD_TYPE: u8 = 7;

/// Resolve a synthetic tile when a recipe references branch 0 and
/// the client sent `0` (no-card sentinel) for that binding. Reads
/// the Zone at `(surface, macro_zone)`, extracts the tile byte at
/// `(q, r)` from `micro_zone`, returns the tile's `packed_definition`,
/// per-row stocks (for stock-aware predicate eval), and a
/// [`HexLocation`] the tape walker uses for tile-side modify ops.
fn derive_synthetic_hex(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,
) -> Option<(u16, (u8, u8), HexLocation)> {
    if surface < SYNTHETIC_HEX_MIN_SURFACE {
        return None;
    }
    let (q, r, _state) = unpack_micro_zone(micro_zone);
    let zone = ctx
        .db
        .zones()
        .macro_zone()
        .filter(macro_zone)
        .filter(|z| z.surface == surface)
        .max_by_key(|z| crate::packed::valid_at_time(z.valid_at))?;
    let (def_id, stock0, stock1) = zone.tile_at(r, q)?;
    if def_id == 0 {
        return None;
    }
    let packed_def = crate::packed::pack_definition(TILE_CARD_TYPE, def_id);
    Some((
        packed_def,
        (stock0, stock1),
        HexLocation {
            zone_id: zone.zone_id,
            macro_zone,
            col: q,
            row: r,
            owner_id: zone.owner_id,
        },
    ))
}

// ----- propose_action reducer ------------------------------------------

/// Verify a client-proposed recipe + bindings, and (eventually)
/// stitch the chain, apply locks, schedule completion. See module
/// docs for the four-stage flow.
///
/// **Status:** Stage 1 verifier landed; Stages 2-4 stubbed.
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn propose_action(
    ctx: &ReducerContext,
    recipe_id: u16,
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,
    root: u32,
    bindings: Vec<Vec<u32>>,
) -> Result<(), String> {
    let caller_player_id = players::resolve_caller(ctx)?;

    let recipe_ref = recipe(recipe_id)
        .map_err(|e| format!("recipe lookup: {e}"))?
        .ok_or_else(|| format!("recipe {recipe_id} not registered"))?;

    if bindings.len() != recipe_ref.iterators.len() {
        return Err(format!(
            "bindings length {} doesn't match recipe iterator count {}",
            bindings.len(),
            recipe_ref.iterators.len(),
        ));
    }

    // If the recipe references branch 0 and the client sent 0
    // (sentinel) for that binding, resolve a synthetic tile from
    // zone data. Only valid when bindings is exactly [0].
    let synthetic_hex = resolve_synthetic_if_needed(
        ctx,
        recipe_ref,
        &bindings,
        surface,
        macro_zone,
        micro_zone,
    )?;

    // Stage 1 — Recipe-vs-stack verification.
    verify_input(ctx, recipe_ref, root, &bindings, synthetic_hex.as_ref())?;

    // Stage 2 — Stack-vs-world cross-check.
    validate_bindings(ctx, recipe_ref, recipe_id, &bindings, caller_player_id)?;

    // Stage 3 — Chain-stitch (write per-card position bytes for root
    // and every top-level iterator binding).
    let now_ms = cards::now_ms(ctx);
    chain_stitch(
        ctx,
        recipe_ref,
        root,
        surface,
        macro_zone,
        micro_zone,
        &bindings,
    )?;

    // Stage 4 — Locks (propose-time claims).
    apply_locks(recipe_ref, &bindings, root, now_ms, ctx);

    // Run the tape walker — emits completion-time future-stamped
    // writes (destroy / create / lock release) at
    // `now_ms + walker.duration * 1000`. The walker computes
    // `walker.duration` from `sys.duration.set` statements in the
    // output tape.
    action_completion::apply(
        ctx,
        recipe_ref,
        &bindings,
        root,
        synthetic_hex.map(|(_, _, loc)| loc),
        now_ms,
        caller_player_id,
    )?;

    Ok(())
}

/// If the recipe references branch 0 at top-level and the client
/// sent the no-card sentinel (`bindings[iter][0] == 0`), resolve a
/// synthetic tile from zone data. Returns `Ok(None)` when no
/// synthesis is needed (either branch 0 isn't referenced or the
/// client provided a real card_id).
fn resolve_synthetic_if_needed(
    ctx: &ReducerContext,
    recipe: &Recipe,
    bindings: &[Vec<u32>],
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,
) -> Result<Option<(u16, (u8, u8), HexLocation)>, String> {
    for (iter_id, it) in recipe.iterators.iter().enumerate() {
        if it.parent.is_empty() && it.branch == 0 {
            let binding = bindings.get(iter_id).ok_or_else(|| {
                format!("bindings missing entry for iterator {iter_id}")
            })?;
            // Only the first slot of branch 0 supports synthetic
            // tiles. If the recipe uses slot.0.1+, those must be
            // real cards.
            if !binding.is_empty() && binding[0] == 0 {
                let synth = derive_synthetic_hex(ctx, surface, macro_zone, micro_zone)
                    .ok_or_else(|| {
                        format!(
                            "recipe references branch 0 with synthetic sentinel \
                             (0) but no tile resolves at (surface={surface}, \
                             macro_zone={macro_zone}, micro_zone={micro_zone})"
                        )
                    })?;
                return Ok(Some(synth));
            }
        }
    }
    Ok(None)
}

// ----- Stage 1: recipe-vs-stack verifier ------------------------------

/// Walk every input statement and evaluate its predicate. Errors
/// are prefixed with the statement index for findability.
fn verify_input(
    ctx: &ReducerContext,
    recipe: &Recipe,
    root: u32,
    bindings: &[Vec<u32>],
    synthetic_hex: Option<&(u16, (u8, u8), HexLocation)>,
) -> Result<(), String> {
    for (i, stmt) in recipe.input.iter().enumerate() {
        verify_stmt(ctx, recipe, stmt, root, bindings, synthetic_hex)
            .map_err(|e| format!("input[{i}]: {e}"))?;
    }
    Ok(())
}

/// Evaluate one input predicate. Path-first grammar — the last
/// segment is the predicate op:
///
/// - `<path>.def_id: <key>` — card def's key equals `<key>`.
/// - `<path>.aspect.<name>.min: <N>` — card's aspect value (or
///   matching stock total) is ≥ `<N>`.
fn verify_stmt(
    ctx: &ReducerContext,
    recipe: &Recipe,
    stmt: &resonantdust_content::recipe_core::Stmt,
    root: u32,
    bindings: &[Vec<u32>],
    synthetic_hex: Option<&(u16, (u8, u8), HexLocation)>,
) -> Result<(), String> {
    let segs = stmt.segments.as_slice();
    let op = segs
        .last()
        .and_then(|s| match s {
            Seg::Word(w) => Some(w.as_str()),
            _ => None,
        })
        .ok_or_else(|| "empty path or non-word terminal segment".to_string())?;

    match op {
        "def_id" => {
            let key = match &stmt.value {
                Some(StatementValue::Str(s)) => s.as_str(),
                _ => return Err("def_id: requires a string value".to_string()),
            };
            let target = &segs[..segs.len() - 1];
            let (packed_def, _stocks) = resolve_target(
                ctx,
                recipe,
                target,
                root,
                bindings,
                synthetic_hex,
            )?;
            let def = decode_definition(packed_def)
                .map_err(|e| format!("decode card def: {e}"))?
                .ok_or_else(|| format!("def_id check: packed {packed_def:#06x} has no def"))?;
            if def.key != key {
                return Err(format!("def_id: expected {key:?}, got {:?}", def.key));
            }
            Ok(())
        }
        "min" => {
            // Shape: <path>.aspect.<name>.min: <N>
            if segs.len() < 4 {
                return Err(format!(
                    "min predicate expects `<path>.aspect.<name>.min`, got {segs:?}"
                ));
            }
            match &segs[segs.len() - 3] {
                Seg::Word(w) if w == "aspect" => {}
                other => {
                    return Err(format!(
                        "min predicate expects `aspect` before name; got {other:?}"
                    ))
                }
            }
            let aspect_name = match &segs[segs.len() - 2] {
                Seg::Word(w) => w.as_str(),
                other => {
                    return Err(format!(
                        "min predicate expects an aspect name; got {other:?}"
                    ))
                }
            };
            let min_value = match &stmt.value {
                Some(StatementValue::Int(n)) => *n as i32,
                _ => return Err("min: requires an integer value".to_string()),
            };
            let target = &segs[..segs.len() - 3];
            let aspect_id = aspect_id(aspect_name)
                .map_err(|e| format!("aspect lookup: {e}"))?
                .ok_or_else(|| format!("unknown aspect {aspect_name:?}"))?;
            let (packed_def, stocks) = resolve_target(
                ctx,
                recipe,
                target,
                root,
                bindings,
                synthetic_hex,
            )?;
            let total = aspect_total(packed_def, aspect_id, stocks)?;
            if total < min_value {
                return Err(format!(
                    "aspect {aspect_name:?}.min: required >= {min_value}, got {total}"
                ));
            }
            Ok(())
        }
        "destroy" | "create" | "set" | "add" | "sub" | "random" => Err(format!(
            "{op:?} is an output op; not valid in input statements"
        )),
        "gt" | "ge" | "lt" | "le" | "eq" | "ne" => Err(format!(
            "{op:?} comparison ops are output-side gates; \
             input predicates use `min` or `def_id`"
        )),
        other => Err(format!("unsupported predicate op {other:?}")),
    }
}

/// Sum aspect values visible to the predicate matcher. Walks the
/// def's stock slots and static aspects, summing every entry whose
/// aspect IS or descends from the target. Per-row stocks (when
/// provided) take precedence when a matching stock slot exists.
fn aspect_total(
    packed_def: u16,
    aspect: AspectId,
    stocks: Option<(u8, u8)>,
) -> Result<i32, String> {
    let def = decode_definition(packed_def)
        .map_err(|e| format!("decode def: {e}"))?
        .ok_or_else(|| format!("packed {packed_def:#06x} has no def"))?;
    if let Some((s0, s1)) = stocks {
        let mut had_match = false;
        let mut stock_total: i32 = 0;
        for (idx, slot) in def.stock.iter().enumerate() {
            if is_aspect_descendant(slot.aspect_id, aspect).unwrap_or(false) {
                had_match = true;
                let row_val = if idx == 0 { s0 } else { s1 } as i32;
                stock_total += row_val;
            }
        }
        if had_match {
            return Ok(stock_total);
        }
    }
    let total: i32 = def
        .aspects
        .iter()
        .filter(|(a, _)| is_aspect_descendant(*a, aspect).unwrap_or(false))
        .map(|(_, v)| *v)
        .sum();
    Ok(total)
}

/// Resolve a segment path to the target's `(packed_definition,
/// stocks)`. Walks anchor + slot refs + `.owner` / `.parent` chain
/// steps, enforcing transition constraints as it goes:
///
/// - **After `.owner`:** the next card's `card_id` must equal the
///   previous card's `owner_id` (the `.owner` step said "go to the
///   card whose id is `prev.owner_id`"; verify the binding agrees).
/// - **After `.parent`:** the next card's `card_id` must equal the
///   previous card's `micro_location` (chain parent pointer).
/// - **Branch constraint on nested iterators:** the resolved card's
///   `micro_zone` direction must match the iterator's `branch`.
///
/// These are all O(1) per segment — no inventory walks, no chain
/// scans. The client pre-resolved every binding; the server just
/// confirms the relationships claimed by the path actually hold.
fn resolve_target(
    ctx: &ReducerContext,
    recipe: &Recipe,
    path: &[Seg],
    root: u32,
    bindings: &[Vec<u32>],
    synthetic_hex: Option<&(u16, (u8, u8), HexLocation)>,
) -> Result<(u16, Option<(u8, u8)>), String> {
    // Resolve the first segment to a card_id. Either `root` or a
    // top-level `Slot` reference.
    let mut card_id = match path.first() {
        Some(Seg::Word(w)) if w == "root" => {
            if root == 0 {
                return Err("root anchor: root is 0".to_string());
            }
            root
        }
        Some(Seg::Slot {
            iterator_id,
            offset,
        }) => {
            let it = recipe
                .iterators
                .get(*iterator_id as usize)
                .ok_or_else(|| format!("iterator_id {iterator_id} out of range"))?;
            let binding_row = bindings
                .get(*iterator_id as usize)
                .ok_or_else(|| format!("bindings missing entry for iterator {iterator_id}"))?;
            let resolved = binding_row.get(*offset as usize).copied().ok_or_else(|| {
                format!(
                    "iterator {iterator_id} offset {offset} out of range (binding len {})",
                    binding_row.len()
                )
            })?;
            // Synthetic-tile case: branch 0, top-level, sentinel 0.
            // Only valid as a leaf (path.len() == 1) — synthetic
            // tiles have no chain to walk.
            if resolved == 0 && it.parent.is_empty() && it.branch == 0 && *offset == 0 {
                if let Some(&(packed, stocks, _)) = synthetic_hex {
                    if path.len() == 1 {
                        return Ok((packed, Some(stocks)));
                    }
                    return Err(
                        "synthetic tile doesn't support owner/parent chain navigation"
                            .to_string(),
                    );
                }
            }
            if resolved == 0 {
                return Err(format!(
                    "iterator {iterator_id} offset {offset}: binding is 0 (no-card sentinel)"
                ));
            }
            resolved
        }
        Some(other) => {
            return Err(format!("unsupported top-level anchor segment {other:?}"))
        }
        None => return Err("empty path".to_string()),
    };

    // Walk subsequent segments. Track what's expected of the next
    // card after each `.owner` / `.parent` transition.
    enum Expect {
        Anything,
        OwnerOf(u32),  // next card.card_id must equal this value
        ParentOf(u32), // next card.card_id must equal this value
    }
    let mut expect = Expect::Anything;
    let mut i = 1;
    while i < path.len() {
        match &path[i] {
            Seg::Word(w) if w == "owner" => {
                let card = cards::latest(ctx, card_id)
                    .ok_or_else(|| format!("card {card_id} not found"))?;
                if card.owner_id == 0 {
                    return Err(format!(
                        "owner step: card {card_id} has no owner"
                    ));
                }
                expect = Expect::OwnerOf(card.owner_id);
                card_id = card.owner_id;
                i += 1;
            }
            Seg::Word(w) if w == "parent" => {
                let card = cards::latest(ctx, card_id)
                    .ok_or_else(|| format!("card {card_id} not found"))?;
                if card.micro_location == 0 {
                    return Err(format!(
                        "parent step: card {card_id} has no parent"
                    ));
                }
                expect = Expect::ParentOf(card.micro_location);
                card_id = card.micro_location;
                i += 1;
            }
            Seg::Slot {
                iterator_id,
                offset,
            } => {
                let it = recipe
                    .iterators
                    .get(*iterator_id as usize)
                    .ok_or_else(|| format!("iterator_id {iterator_id} out of range"))?;
                let binding_row = bindings.get(*iterator_id as usize).ok_or_else(|| {
                    format!("bindings missing entry for iterator {iterator_id}")
                })?;
                let resolved = binding_row.get(*offset as usize).copied().ok_or_else(|| {
                    format!(
                        "iterator {iterator_id} offset {offset} out of range \
                         (binding len {})",
                        binding_row.len()
                    )
                })?;
                if resolved == 0 {
                    return Err(format!(
                        "iterator {iterator_id} offset {offset}: binding is 0"
                    ));
                }
                // A Slot reference following `.owner` / `.parent`
                // means "look up this iterator's binding, which is a
                // card in the chain rooted at the owner/parent we
                // just walked to." The resolved card is in that
                // chain, not the owner/parent itself — so
                // `resolved == card_id` is the wrong check (it
                // confused "iterator parent target" with "iterator
                // binding"). The structural correctness is verified
                // by the branch-direction check below plus the
                // ownership / FLAG_SLOT_HOLD checks in
                // `validate_bindings`; the Expect mechanism was just
                // wrong-headed here.
                //
                // The one case we still want to reject is a
                // Slot-after-Slot with no `.owner` / `.parent`
                // between — that's a parser-level malformed path.
                if matches!(expect, Expect::Anything) && i > 1 {
                    if let Seg::Slot { .. } = &path[i - 1] {
                        return Err(format!(
                            "unexpected slot reference (iter {iterator_id}, \
                             offset {offset}) without prior owner/parent step"
                        ));
                    }
                }
                // For nested iterators (parent != []), the bound card
                // must actually be in the right branch. Read its
                // micro_zone direction and check against
                // `iterator.branch`. (Top-level iterators get their
                // branch direction stamped by chain-stitch at Stage 3
                // — no check needed here since the server is the one
                // writing it.)
                if !it.parent.is_empty() {
                    let card = cards::latest(ctx, resolved)
                        .ok_or_else(|| format!("card {resolved} not found"))?;
                    let actual_dir = micro_zone_direction(card.micro_zone);
                    if actual_dir != it.branch {
                        return Err(format!(
                            "branch mismatch: iterator {iterator_id} expects \
                             branch {}, but card {resolved}'s actual direction \
                             is {}",
                            it.branch, actual_dir
                        ));
                    }
                }
                card_id = resolved;
                expect = Expect::Anything;
                i += 1;
            }
            other => {
                return Err(format!(
                    "unsupported path segment {other:?} in chain navigation"
                ))
            }
        }
    }

    let card = cards::latest(ctx, card_id)
        .ok_or_else(|| format!("resolve target: card {card_id} not found"))?;
    Ok((card.packed_definition, None))
}

// ----- Stage 2: stack-vs-world validation -----------------------------

/// Cross-check every card_id in `bindings` against the cards table.
///
/// For each non-sentinel card:
/// - The row must exist and not be dead.
/// - `slot_hold` must be clear — claimed cards can't be re-claimed
///   by a concurrent action.
/// - Ownership must lead back to the caller (or to the world
///   anonymous player for shared-world cards).
/// - Magnetic discipline: if a bound card carries the magnetic
///   flag, the `recipe_id` must equal the card def's declared
///   `magnetic.recipe`. This is what prevents a client from
///   resolving a magnetic card with the wrong recipe.
///
/// Duplicate detection covers the whole bindings 2-D array; the
/// same card can't appear at two different (iter, offset) slots.
/// Card_id 0 is the synthetic-tile sentinel and is skipped.
fn validate_bindings(
    ctx: &ReducerContext,
    recipe: &Recipe,
    recipe_id: u16,
    bindings: &[Vec<u32>],
    caller_player_id: u32,
) -> Result<(), String> {
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    for (iter_id, binding_row) in bindings.iter().enumerate() {
        let iter_locks = recipe
            .iterators
            .get(iter_id)
            .map(|it| it.slot_hold)
            .unwrap_or(true);
        for &card_id in binding_row.iter() {
            if card_id == 0 {
                continue;
            }
            if !seen.insert(card_id) {
                return Err(format!(
                    "card {card_id} appears more than once in bindings"
                ));
            }
            let card = cards::latest(ctx, card_id)
                .ok_or_else(|| format!("card {card_id} not found"))?;
            if card.flags & FLAG_DEAD != 0 {
                return Err(format!("card {card_id} is dead"));
            }
            // Borrow iterators (lookup-only, no `slot_hold` claim)
            // skip the in-flight check so concurrent recipes can
            // simultaneously borrow the same tool — e.g. two
            // cut_tree actions running in parallel both referencing
            // the soul's axe. Non-borrow (locked) iterators still
            // reject if their bindings are already slot_held by
            // another action.
            if iter_locks && card.flags & FLAG_SLOT_HOLD != 0 {
                return Err(format!(
                    "card {card_id} is already claimed by another in-flight action"
                ));
            }
            // Ownership: walk up to the responsible player. Either
            // the caller or the world-anonymous bucket is fine.
            let owner_player =
                cards::owning_player(ctx, card_id).unwrap_or(cards::WORLD_PLAYER_ID);
            if owner_player != caller_player_id
                && owner_player != cards::WORLD_PLAYER_ID
            {
                return Err(format!(
                    "card {card_id} is owned by player {owner_player}, not caller {caller_player_id}"
                ));
            }
            // Magnetic discipline.
            if card.flags & FLAG_LIFECYCLE_PENDING != 0 {
                let def = decode_definition(card.packed_definition)
                    .map_err(|e| format!("decode def for magnetic check: {e}"))?
                    .ok_or_else(|| format!("card {card_id} has unknown def"))?;
                let expected = lifecycle_recipe_for_def(def)
                    .map_err(|e| format!("magnetic recipe lookup: {e}"))?
                    .ok_or_else(|| {
                        format!(
                            "card {card_id} carries magnetic flag but def declares no magnetic recipe"
                        )
                    })?;
                if expected != recipe_id {
                    return Err(format!(
                        "card {card_id} is magnetic-locked to recipe {expected}, got {recipe_id}"
                    ));
                }
            }
        }
    }
    Ok(())
}

// ----- Stage 3: chain-stitch -----------------------------------------

/// Write per-card position bytes so the chain is a server-side
/// fact, visible to all subscribers. Root lands `Free` at the
/// proposed `(surface, macro_zone, micro_zone)`; each top-level
/// iterator's bindings stitch into branch `iterator.branch` as a
/// parent-pointer chain off root.
///
/// Card layout per branch:
/// - `bindings[iter][0]` — `state=OnRoot, direction=branch,
///   position=1, micro_location=root`.
/// - `bindings[iter][i>0]` — `state=Slot, direction=branch,
///   micro_location=bindings[iter][i-1]` (parent pointer chain).
///
/// Nested iterators (parent != []) are left alone — their cards
/// already live in other chains (equipment, etc.). Synthetic-tile
/// sentinels (card_id == 0) are skipped.
fn chain_stitch(
    ctx: &ReducerContext,
    recipe: &Recipe,
    root: u32,
    surface: u8,
    macro_zone: u32,
    micro_zone: u8,
    bindings: &[Vec<u32>],
) -> Result<(), String> {
    if root != 0 {
        let (q, r, _) = unpack_micro_zone(micro_zone);
        let root_micro_zone = pack_micro_zone(q, r, StackedState::Free);
        cards::update_with(ctx, root, |c| {
            c.surface = surface;
            c.macro_zone = macro_zone;
            c.micro_zone = root_micro_zone;
            // Free state uses micro_location for [x, y] pixel coords;
            // for action-rooted cards we don't carry sub-tile coords,
            // so 0 it.
            c.micro_location = 0;
            c.flags |= FLAG_FORCE_POSITION;
        });
    }

    for (iter_id, it) in recipe.iterators.iter().enumerate() {
        if !it.parent.is_empty() {
            continue;
        }
        let binding_row = &bindings[iter_id];
        for (offset, &card_id) in binding_row.iter().enumerate() {
            if card_id == 0 {
                continue;
            }
            // Root may appear in bindings when the client promoted it
            // to `slot.<branch>.0` for a recipe with `anchors.root ==
            // false` (e.g. `corpus + corpus` matching against a stack
            // of two corpus where the bottom is the loose root).
            // Root's position was already written above; subsequent
            // bindings entries (offset >= 1) will chain off it via
            // `binding_row[offset - 1] == root`, so the visual chain
            // is still correct.
            if card_id == root {
                continue;
            }
            let direction = it.branch;
            let (new_micro_zone, parent_id) = if offset == 0 {
                (
                    pack_stack_micro_zone(1, direction, StackedState::OnRoot),
                    root,
                )
            } else {
                (
                    pack_slot_micro_zone(direction),
                    binding_row[offset - 1],
                )
            };
            cards::update_with(ctx, card_id, |c| {
                c.surface = surface;
                c.macro_zone = macro_zone;
                c.micro_zone = new_micro_zone;
                c.micro_location = parent_id;
                c.flags |= FLAG_FORCE_POSITION;
            });
        }
    }
    Ok(())
}

// ----- Stage 4: locks ------------------------------------------------

/// Apply `slot_hold` to every bound card (claims it for this action)
/// and `position_hold` (ref-counted) to top-level chain cards
/// (preserves chain integrity against movement / other actions).
///
/// Nested-iterator bindings get only `slot_hold` — they already
/// live in their own chains, and that chain's structure isn't this
/// action's responsibility to preserve.
///
/// **Note:** Phase 8 (`action_completion::apply`) will release these
/// locks at completion time. Until Phase 8 lands, locks applied
/// here persist indefinitely; expect to wipe state between dev
/// iterations.
fn apply_locks(
    recipe: &Recipe,
    bindings: &[Vec<u32>],
    root: u32,
    now_ms: u64,
    ctx: &ReducerContext,
) {
    // Hold policy is fully explicit per the parser's prefix tokens
    // (`borrow.` / `share.` / `claim.` / `use.`):
    //   - `slot_hold`     → FLAG_SLOT_HOLD (exclusive claim + blocks user pickup)
    //   - `position_hold` → ref-counted movement lock
    //
    // The parser aggregates per-statement prefixes to per-iterator
    // and per-root tuples via last-write-wins. No more cross-anchor
    // inference here — the recipe author writes what they want.

    // Root: explicit anchor tokens, plus client root-promotion (root
    // may appear in an iterator's bindings when a `slot.X.0` recipe
    // matched against a stack whose bottom is the loose root).
    let mut claim_root = recipe.anchors.root && recipe.root_slot_hold;
    let mut pin_root = recipe.anchors.root && recipe.root_position_hold;
    for (i, it) in recipe.iterators.iter().enumerate() {
        let row = match bindings.get(i) {
            Some(r) => r,
            None => continue,
        };
        if row.iter().any(|&id| id == root) {
            if it.slot_hold {
                claim_root = true;
            }
            if it.position_hold {
                pin_root = true;
            }
        }
    }
    if root != 0 {
        if claim_root {
            cards::update_with(ctx, root, |c| {
                c.flags |= FLAG_SLOT_HOLD;
            });
        }
        if pin_root {
            cards::acquire_position_hold(ctx, root, now_ms);
        }
    }

    for (iter_id, it) in recipe.iterators.iter().enumerate() {
        if !it.slot_hold && !it.position_hold {
            continue;
        }
        for &card_id in &bindings[iter_id] {
            if card_id == 0 {
                continue;
            }
            // Root's locks were applied above (with promotion-aware
            // union across iterators); skip here to avoid double-
            // acquire (would leak the position_hold refcount).
            if card_id == root {
                continue;
            }
            if it.position_hold {
                cards::acquire_position_hold(ctx, card_id, now_ms);
            }
            if it.slot_hold {
                cards::update_with(ctx, card_id, |c| {
                    c.flags |= FLAG_SLOT_HOLD;
                });
            }
        }
    }
}

