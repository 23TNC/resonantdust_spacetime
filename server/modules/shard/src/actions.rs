//! `propose_action` reducer — verifier for the tape-form recipe model.
//!
//! Wire format (per-iterator bindings — no inventory walks):
//!
//! ```text
//! propose_action(
//!   recipe_id: u16,
//!   surface: u8, macro_zone: u64, micro_location: u32,  // root's intended loose location
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
//! sentinel), the server synthesizes from zone tile data at the cell decoded
//! from `(surface, macro_zone, micro_location)`.
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
use crate::cards::{self, Micro};
use crate::flags::state_flags;
use crate::pending_actions;
use crate::packed::{loose_kind_for_surface, micro_loose_cell, unpack_micro_loose};
use crate::players;
// Module + the generated `zones()` accessor trait — needed for the
// synthetic-tile lookup. Same `(self, … as _)` pattern as elsewhere.
use crate::zones::zones as _zones_table;

// Flag bit positions sourced from `content/cards/flags.json` via the
// `state_flags()` / `bk_flags()` caches. Per-module `const FLAG_*`
// declarations were retired when the cache landed — see
// `crate::flags`.

/// Surfaces ≥ this carry hex-tile data inside their backing `Zone` row.
const SYNTHETIC_HEX_MIN_SURFACE: u8 = 32;

/// `card_type` value for tile-cards (promoted zone tiles). Mirrors
/// the constant in `gc.rs` / `world_gen.rs` / `movement.rs` — source
/// of truth lives in `content/cards/types.json` (`tile` = 7).
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
    macro_zone: u64,
    micro_location: u32,
    now_ms: u64,
) -> Option<(u16, (u8, u8), HexLocation)> {
    if surface < SYNTHETIC_HEX_MIN_SURFACE {
        return None;
    }
    let (q, r) = micro_loose_cell(micro_location);
    // Card-priority read: a previously promoted tile-card (which may
    // carry a mutated def — Phase 4+) wins over the Zone slot.
    let (packed_def, stock0, stock1) =
        cards::tile_full_view(ctx, surface, macro_zone, q, r, now_ms)?;
    // HexLocation still references the Zone (zone_id / owner_id) —
    // Phase 4 reroutes writes through cards and drops HexLocation's
    // role here.
    let zone = crate::zones::latest_for(ctx, crate::packed::with_surface(macro_zone, surface))?;
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

/// Verify a client-proposed recipe + bindings, then stitch the
/// chain, apply locks, and run completion. See module docs for the
/// four-stage flow.
///
/// **Time discipline:** `client_time_ms` is the client's view of
/// "now" at submission. The reducer uses [`cards::effective_now_ms`]
/// to resolve a `now_ms` that's `min(client, server)` (within grace
/// bounds); see that helper's doc for the rejection contract. All
/// in-reducer time-reads (verifier lookups, chain-stitch writes,
/// position-hold acquires, action-completion future-stamps) thread
/// this single value, so the row visibilities the client observes
/// stay consistent with the writes the server makes.
///
/// **Status:** all four stages live — verify, existence/liveness,
/// chain-stitch, lock + `action_completion::plan`/`commit`.
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn propose_action(
    ctx: &ReducerContext,
    client_time_ms: u64,
    recipe_id: u16,
    surface: u8,
    macro_zone: u64,
    micro_location: u32,
    root: u32,
    bindings: Vec<Vec<u32>>,
) -> Result<(), String> {
    let outcome = propose_action_inner(
        ctx,
        client_time_ms,
        recipe_id,
        surface,
        macro_zone,
        micro_location,
        root,
        &bindings,
    );
    // Structured propose log — paired accepted/rejected with the
    // `dedup_key` so post-hoc you can match an accepted propose to its
    // completion (which logs nothing today; the registry release in
    // `commit` is the implicit ack) or to a duplicate-rejection
    // referencing the same key. The key is computed inline (cheap hash)
    // so rejections at recipe-lookup / verify_input / validate_bindings
    // — i.e. before the inner ran far enough to compute it — still
    // surface the same shape.
    let dedup_key = pending_actions::dedup_key(recipe_id, root, &bindings);
    match &outcome {
        Ok(()) => log::info!(
            "[propose] verdict=accepted recipe={recipe_id} root={root} dedup_key={dedup_key:#018x}"
        ),
        Err(reason) => log::info!(
            "[propose] verdict=rejected recipe={recipe_id} root={root} dedup_key={dedup_key:#018x} reason={reason:?}"
        ),
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
fn propose_action_inner(
    ctx: &ReducerContext,
    client_time_ms: u64,
    recipe_id: u16,
    surface: u8,
    macro_zone: u64,
    micro_location: u32,
    root: u32,
    bindings: &[Vec<u32>],
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

    // Resolve the time-base for this reducer. May reject with
    // `time_drift:client_(behind|ahead)_by=N` — the client's
    // ActionManager parses these and schedules a retry.
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;

    // If the recipe references branch 0 and the client sent 0
    // (sentinel) for that binding, resolve a synthetic tile from
    // zone data. Only valid when bindings is exactly [0].
    let synthetic_hex = resolve_synthetic_if_needed(
        ctx,
        recipe_ref,
        bindings,
        surface,
        macro_zone,
        micro_location,
        now_ms,
    )?;

    // Promote the synthetic tile to a real Card row and substitute
    // its `card_id` into the matching binding. After this block, the
    // dedup gate, plan-builder, chain-stitch, lock acquire, and
    // commit-time release all operate on a real card; the
    // `synthetic_hex` carrier remains for the legacy verify/validate
    // path until Phases 3/7 retire it. See `docs/TILE_AS_CARD.md`.
    let substituted_bindings: Option<Vec<Vec<u32>>> = if synthetic_hex.is_some() {
        let (q, r) = micro_loose_cell(micro_location);
        let tile_card = cards::find_or_create_tile_card(ctx, surface, macro_zone, q, r, now_ms)
            .map_err(|e| format!("promote tile: {e}"))?;
        let mut new_bindings = bindings.to_vec();
        for (iter_id, it) in recipe_ref.iterators.iter().enumerate() {
            if it.parent.is_empty()
                && it.branch == 0
                && new_bindings
                    .get(iter_id)
                    .and_then(|b| b.first().copied())
                    == Some(0)
            {
                new_bindings[iter_id][0] = tile_card.card_id;
                break;
            }
        }
        Some(new_bindings)
    } else {
        None
    };
    let bindings: &[Vec<u32>] = substituted_bindings.as_deref().unwrap_or(bindings);

    // Stage 1 — Recipe-vs-stack verification.
    verify_input(ctx, recipe_ref, root, &bindings, synthetic_hex.as_ref(), now_ms)?;

    // Stage 2 — Stack-vs-world cross-check.
    validate_bindings(
        ctx, recipe_ref, recipe_id, root, &bindings, caller_player_id, now_ms,
    )?;

    // In-flight dedup gate. Hash the (recipe, root, bindings) tuple
    // and reject if a row already exists in `pending_actions`. Catches
    // every "same exact propose, twice" case regardless of whether
    // the recipe declares a `slot_hold` / `style.set` / any other
    // hold. The row is inserted here and deleted in
    // `action_completion::commit`. Stale rows (commit never ran)
    // are reaped by the GC sweep. See `pending_actions` module doc.
    let dedup_key = pending_actions::dedup_key(recipe_id, root, &bindings);
    if pending_actions::is_in_flight(ctx, dedup_key) {
        return Err(format!(
            "duplicate propose: recipe {recipe_id} root {root} bindings already in flight"
        ));
    }

    // Walk the output tape into an `ActionPlan` so `commit` can emit
    // completion-time writes from the same walk that determined the
    // duration (= `completion_ms - now_ms`). The walk does DB reads
    // but no writes.
    let plan = action_completion::plan(
        ctx,
        recipe_ref,
        bindings,
        root,
        synthetic_hex.as_ref().map(|(_, _, loc)| loc),
    )?;
    let completion_ms = now_ms + plan.duration_ms();
    pending_actions::install(ctx, dedup_key, completion_ms);

    // Stage 3 — Chain-stitch (write per-card position bytes for root
    // and every top-level iterator binding) stamped at `now_ms`.
    chain_stitch(
        ctx,
        recipe_ref,
        root,
        surface,
        macro_zone,
        micro_location,
        bindings,
        now_ms,
    )?;

    // Stage 4 — Locks (propose-time claims). Reads `plan.holds()`
    // so the apply path and commit-time release mirror exactly — see
    // `action_completion::compute_holds`.
    apply_locks(&plan, now_ms, ctx);

    // Emit completion-time future-stamped writes (destroy / create /
    // lock release) at `completion_ms`. The dedup_key is threaded
    // through so commit can release the registry row in the same
    // pass.
    action_completion::commit(
        ctx,
        plan,
        bindings,
        root,
        now_ms,
        caller_player_id,
        dedup_key,
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
    macro_zone: u64,
    micro_location: u32,
    now_ms: u64,
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
                let synth = derive_synthetic_hex(ctx, surface, macro_zone, micro_location, now_ms)
                    .ok_or_else(|| {
                        format!(
                            "recipe references branch 0 with synthetic sentinel \
                             (0) but no tile resolves at (surface={surface}, \
                             macro_zone={macro_zone}, micro_location={micro_location})"
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
///
/// `now_ms` is the effective time-base resolved by
/// [`cards::effective_now_ms`] — passed through to every card lookup
/// so the verifier reads the card state the client thinks it's
/// referencing.
fn verify_input(
    ctx: &ReducerContext,
    recipe: &Recipe,
    root: u32,
    bindings: &[Vec<u32>],
    synthetic_hex: Option<&(u16, (u8, u8), HexLocation)>,
    now_ms: u64,
) -> Result<(), String> {
    for (i, stmt) in recipe.input.iter().enumerate() {
        verify_stmt(ctx, recipe, stmt, root, bindings, synthetic_hex, now_ms)
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
    now_ms: u64,
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
                now_ms,
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
                now_ms,
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
        .map(|(_, v)| *v as i32)
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
    now_ms: u64,
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
                let card = cards::prior_at(ctx, card_id, now_ms)
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
                let card = cards::prior_at(ctx, card_id, now_ms)
                    .ok_or_else(|| format!("card {card_id} not found"))?;
                // Flat chains are depth-1: a member's only "parent" is its root
                // (held in `micro_location`). A loose/root card has no parent —
                // its `micro_location` is packed coords, not a card_id.
                if !cards::micro_is_card(&card) {
                    return Err(format!(
                        "parent step: card {card_id} has no parent (it is a chain root)"
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
                    let card = cards::prior_at(ctx, resolved, now_ms)
                        .ok_or_else(|| format!("card {resolved} not found"))?;
                    let actual_dir = cards::stack_branch(&card);
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

    let card = cards::prior_at(ctx, card_id, now_ms)
        .ok_or_else(|| format!("resolve target: card {card_id} not found"))?;
    // For tile-cards (the promoted-zone-tile family), surface
    // `tile_stock_{0,1}` from `flags_bk` so stock-bound aspect
    // predicates resolve against the card's current stock — same
    // shape the legacy `synthetic_hex` branch above returns.
    // Non-tile cards report `None` (their def.stock entries, if any,
    // never had a row-stock channel pre-tile-as-card).
    let (card_type, _) = crate::packed::unpack_definition(card.packed_definition);
    let stocks = if card_type == TILE_CARD_TYPE {
        Some((
            cards::tile_stock(card.flags_bk, 0),
            cards::tile_stock(card.flags_bk, 1),
        ))
    } else {
        None
    };
    Ok((card.packed_definition, stocks))
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
///
/// **Card_id 0 handling.** Under the tile-as-card rework
/// (`docs/TILE_AS_CARD.md`), `propose_action_inner` substitutes the
/// branch-0 sentinel with the promoted tile-card's `card_id` BEFORE
/// this function runs — so in normal flow no `0` reaches this loop.
/// The `if card_id == 0 { continue; }` guard remains defensively
/// for any future code path that might pass an unsubstituted 0 (e.g.,
/// a recipe shape with `0` in a non-branch-0 position).
fn validate_bindings(
    ctx: &ReducerContext,
    recipe: &Recipe,
    recipe_id: u16,
    root: u32,
    bindings: &[Vec<u32>],
    caller_player_id: u32,
    now_ms: u64,
) -> Result<(), String> {
    // Root: dead-flag gate + hold-kind gate. The hold-kind matrix
    // mirrors the per-binding loop below but reads `recipe.root_slot_hold`
    // in place of `iter.slot_hold`. Duplicate-detection of the exact
    // same propose tuple is owned by the `pending_actions` registry
    // gate in `propose_action`; the hold-kind gate here is what catches
    // **conflicting** in-flight actions on the same root.
    // Hold-kind gate matrix (unified-counts era):
    // - `slot_hold_count > 0` means an exclusive (`claim.` / `use.`)
    //   hold is in flight — rejects everyone.
    // - `slot_share_count > 0` means shared (`borrow.` / `share.`)
    //   holds are in flight — rejects only new claims, not borrows.
    // - `touch_count >= TOUCH_COUNT_CLIENT_CAP` means the per-card
    //   client-concurrency ceiling is hit — rejects everyone.
    //   Caps the hold-count fields well below their u3 saturation
    //   under realistic gameplay.
    let s = state_flags();
    if root != 0 {
        let card = cards::prior_at(ctx, root, now_ms)
            .ok_or_else(|| format!("root card {root} not found"))?;
        if card.flags_state & s.dead != 0 {
            return Err(format!("root card {root} is dead"));
        }
        if cards::slot_hold_count(card.flags_bk) > 0 {
            return Err(format!(
                "root card {root} is exclusively held by another in-flight action"
            ));
        }
        if recipe.root_slot_hold && cards::slot_share_count(card.flags_bk) > 0 {
            return Err(format!(
                "root card {root} is shared-held by another in-flight action; cannot claim"
            ));
        }
        if cards::touch_count(card.flags_bk) >= crate::flags::TOUCH_COUNT_CLIENT_CAP {
            return Err(format!(
                "root card {root} has too many concurrent in-flight actions (cap {})",
                crate::flags::TOUCH_COUNT_CLIENT_CAP
            ));
        }
        // Drop-onto-target gate: chain_stitch will parent every
        // top-level iterator binding onto root via the bindings'
        // `micro_location = root` write. A drop-locked root rejects
        // every such stitch attempt.
        if cards::drop_hold_count(card.flags_bk) > 0 {
            return Err(format!(
                "root card {root} blocks stacking (drop_hold_count > 0)"
            ));
        }
    }

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
            let card = cards::prior_at(ctx, card_id, now_ms)
                .ok_or_else(|| format!("card {card_id} not found"))?;
            if card.flags_state & s.dead != 0 {
                return Err(format!("card {card_id} is dead"));
            }
            if cards::slot_hold_count(card.flags_bk) > 0 {
                return Err(format!(
                    "card {card_id} is exclusively held by another in-flight action"
                ));
            }
            if iter_locks && cards::slot_share_count(card.flags_bk) > 0 {
                return Err(format!(
                    "card {card_id} is shared-held by another in-flight action; cannot claim"
                ));
            }
            if cards::touch_count(card.flags_bk) >= crate::flags::TOUCH_COUNT_CLIENT_CAP {
                return Err(format!(
                    "card {card_id} has too many concurrent in-flight actions (cap {})",
                    crate::flags::TOUCH_COUNT_CLIENT_CAP
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
            if card.flags_state & s.magnetic != 0 {
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

/// Write per-card position so the chain is a server-side fact, visible to all
/// subscribers. Root lands loose at the proposed `(surface, macro_zone,
/// micro_location)`; each top-level iterator's bindings become flat members of
/// `root` in branch `iterator.branch`, indexed by their offset.
///
/// Flat-root layout per branch:
/// - `bindings[iter][offset]` — `Micro::Stacked{ root, branch=iterator.branch,
///   index=offset }`. No parent pointers — every member points straight at root.
///
/// Nested iterators (parent != []) are left alone — their cards already live in
/// other chains (equipment, etc.). Synthetic-tile sentinels (card_id == 0) and
/// the root itself (if it appears in bindings) are skipped.
fn chain_stitch(
    ctx: &ReducerContext,
    recipe: &Recipe,
    root: u32,
    surface: u8,
    macro_zone: u64,
    micro_location: u32,
    bindings: &[Vec<u32>],
    now_ms: u64,
) -> Result<(), String> {
    let full_macro = crate::packed::with_surface(macro_zone, surface);
    let pos_need = state_flags().pos_need;

    if root != 0 {
        // Root lands loose at the proposed cell (decoded from the wire
        // micro_location), kind by surface.
        let (q, r, x, y) = unpack_micro_loose(micro_location);
        let root_micro = Micro::Loose {
            local_q: q,
            local_r: r,
            x,
            y,
            kind: loose_kind_for_surface(surface),
        };
        cards::update_with_at(ctx, root, now_ms, |c| {
            c.macro_zone = full_macro;
            root_micro.apply(c);
            c.flags_state |= pos_need;
        });
    }

    for (iter_id, it) in recipe.iterators.iter().enumerate() {
        if !it.parent.is_empty() {
            continue;
        }
        let binding_row = &bindings[iter_id];
        for (offset, &card_id) in binding_row.iter().enumerate() {
            if card_id == 0 || card_id == root {
                continue;
            }
            let member = Micro::Stacked {
                root,
                branch: it.branch,
                index: (offset as u8).min(15),
            };
            cards::update_with_at(ctx, card_id, now_ms, |c| {
                c.macro_zone = full_macro;
                member.apply(c);
                c.flags_state |= pos_need;
            });
        }
    }
    Ok(())
}

// ----- Stage 4: locks ------------------------------------------------

/// Acquire the hold flavors `plan.holds()` declares for every
/// touched card, at `now_ms`. Three independent fields per card:
///
/// - `slot_hold` (single bit, exclusive) — set for `claim.` / `use.`
///   iterators. Validate_bindings rejects new claims/uses if this bit
///   is set on any of their bindings.
/// - `slot_share_count` (3-bit refcount, multi-holder) — incremented
///   for `borrow.` / `share.` iterators. Validate_bindings rejects
///   new claims/uses if `count > 0`; new borrows are allowed
///   (increment further) as long as `slot_hold == 0`.
/// - `position_hold_count` (3-bit refcount) — incremented per
///   `it.position_hold == true`. Used by the position-pin /
///   chain-stitch infrastructure to block movement.
///
/// Reads the (card_id → HoldKinds) map from the plan rather than
/// re-deriving inline so the acquire pass here and the release pass
/// in `action_completion::commit` cannot drift — same map, same
/// keys, same kinds.
fn apply_locks(
    plan: &action_completion::ActionPlan,
    now_ms: u64,
    ctx: &ReducerContext,
) {
    for (&card_id, kinds) in plan.holds() {
        if card_id == 0 {
            continue;
        }
        // `touch_count` increments once per recipe per card — the
        // map is already per-card deduplicated by `compute_holds`,
        // so root-promotion paths don't double-count. The cap is
        // enforced in `validate_bindings`; by the time we reach
        // here, the cap is known clear.
        cards::acquire_touch(ctx, card_id, now_ms);
        if kinds.slot_hold {
            cards::acquire_slot_hold(ctx, card_id, now_ms);
        }
        if kinds.slot_share {
            cards::acquire_slot_share(ctx, card_id, now_ms);
        }
        if kinds.position_hold {
            cards::acquire_position_hold(ctx, card_id, now_ms);
        }
    }
}

