//! Shared recipe-evaluation primitives.
//!
//! - **`aspect_pool`** — aggregate aspect values across a sequence of
//!   card defs, used by conditional-duration resolution.
//! - **`entity_satisfied_pool`** — boolean evaluator: does this pool of
//!   aspects satisfy this entity? Used by both `propose_action` (pool
//!   = root + slots) and `on_create::trigger` (pool = the new card).
//! - **`entity_specificity`** — integer score of how specifically an
//!   entity matches a single card def. Used by `on_create::trigger`
//!   for picking the most-specific OnCreate recipe and by
//!   `magnetic::magnetic_tick` to rank candidate cards in the player's
//!   inventory for a magnetic pull.
//! - **`soul_stack`** — collect the cards on one branch (UP or DOWN)
//!   of a soul card's stack. Used by `has` / `reagents.has`
//!   (UP — equipment) and `has_below` / `reagents.has_below`
//!   (DOWN — action stack / debuffs) predicate resolution.
//!
//! These are kept here rather than in `resonantdust_content::recipe_core`
//! because they're consumer-side: how the spacetime module *uses*
//! recipes against the cards table.

use std::collections::{BTreeMap, BTreeSet};

use resonantdust_content::definition_core::{decode_definition, AspectId, CardDefinition};
use resonantdust_content::recipe_core::{Entity, HasOps, RecipeDef};
use spacetimedb::ReducerContext;

use crate::cards::{self, cards as _cards_table, Card};
use crate::packed::{
    micro_zone_direction, unpack_micro_zone, StackedState, STACK_DIR_DOWN, STACK_DIR_UP,
};

// `cards/flags.json` bit positions used by the stack walker / hold
// writes. Kept local rather than promoted; this is the only file that
// needs them.
const FLAG_DEAD: u32 = 1 << 7;
const FLAG_SLOT_HOLD: u32 = 1 << 5;

/// Max depth of the soul-stack walk. Bounds pathological chains and
/// keeps `has` predicate resolution O(1) in chain length under normal
/// gameplay. The "top stack = equipment" convention typically tops out
/// at handful of cards; 16 leaves comfortable slack.
const SOUL_STACK_MAX_DEPTH: usize = 16;

/// Sum aspect values across a sequence of card defs into a single map.
///
/// Used by conditional-duration evaluation (see [`entity_satisfied_pool`]).
/// Aspects not present in any def are absent from the map; callers
/// that need a "0 if missing" view should
/// `.get(...).copied().unwrap_or(0)`.
///
/// Generic over the iterable so both single-card pools (OnCreate's new
/// card on its own) and multi-card pools (a stack recipe's root + slots)
/// share the same aggregator.
pub fn aspect_pool<'a, I>(defs: I) -> BTreeMap<AspectId, i32>
where
    I: IntoIterator<Item = &'a CardDefinition>,
{
    let mut pool: BTreeMap<AspectId, i32> = BTreeMap::new();
    for def in defs {
        for (aspect, value) in &def.aspects {
            *pool.entry(*aspect).or_insert(0) += value;
        }
    }
    pool
}

/// Boolean: does the aspect pool satisfy this entity's condition?
///
/// Restricted to aspect-shape entities — the pool is an aggregate,
/// not a single card, so `Card` and `Type` entities have no meaning
/// here and return `Err`. `Any` is trivially true; `And` / `Or` /
/// `WeightedOr` recurse over the standard short-circuit logic.
pub fn entity_satisfied_pool(
    entity: &Entity,
    pool: &BTreeMap<AspectId, i32>,
) -> Result<bool, String> {
    match entity {
        Entity::Aspect(aspect, min) => {
            let val = pool.get(aspect).copied().unwrap_or(0);
            Ok(val >= *min)
        }
        Entity::Any => Ok(true),
        Entity::And(children) => {
            for c in children {
                if !entity_satisfied_pool(c, pool)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Entity::Or(children) => {
            for c in children {
                if entity_satisfied_pool(c, pool)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Entity::Not(child) => Ok(!entity_satisfied_pool(child, pool)?),
        Entity::WeightedOr { a, b, .. } => {
            Ok(entity_satisfied_pool(a, pool)? || entity_satisfied_pool(b, pool)?)
        }
        Entity::Card(_)
        | Entity::Type(_)
        | Entity::Category(_)
        | Entity::Flag(_) => Err(
            "conditional duration entities can only reference aspects \
             (or boolean combinators of them); card / type / category / \
             flag predicates aren't meaningful in an aspect-pool context"
                .to_string(),
        ),
    }
}

/// Score how well a single entity matches a card definition. `0` means
/// the entity is not satisfied by this card; positive values reflect
/// match specificity, higher = more specific. Used by callers that need
/// to pick the "most specific match" out of a candidate set:
/// `on_create::trigger` ranks candidate OnCreate recipes against the new
/// card, and `magnetic::magnetic_tick` ranks candidate inventory cards
/// against a magnetic recipe's pull target.
///
/// Scoring (kept in sync with the original private copy in `on_create.rs`):
///
/// - `Card`   — `4` on exact card-key match.
/// - `Aspect` — `3` when the def's aspect value clears the entity's min.
/// - `Type`   — `2` on type match.
/// - `Any`    — `1` (always matches).
/// - `And`    — sum of children if both match (non-zero), else `0`.
/// - `Or` / `WeightedOr` — max of children's specificities.
///
/// Mirrors the private `entity_specificity` in the content crate; kept
/// consumer-side because the spacetime module is the only caller today.
pub fn entity_specificity(entity: &Entity, def: &CardDefinition) -> u32 {
    // Delegate to the content-crate canonical implementation. The
    // server-side function exists as a re-export wrapper so consumers
    // here don't need to import from `resonantdust_content::recipe_core`
    // directly — and so callers like `on_create::trigger` that need
    // specificity-based ranking go through one shared definition.
    resonantdust_content::recipe_core::entity_specificity(entity, def)
}

/// Walk one branch of a soul card's stack and return every alive
/// card resting on it, ordered breadth-first from the soul outward
/// (so the immediate root-stacked child appears first, its child
/// second, etc.). Used by `has` / `reagents.has` (UP — equipment)
/// and `has_below` / `reagents.has_below` (DOWN — action stack /
/// debuffs) predicate resolution.
///
/// `direction` is `STACK_DIR_UP` or `STACK_DIR_DOWN` — the recipe's
/// stack-direction convention: things on top of the soul go UP,
/// queued actions / debuffs hang DOWN. Both branches are valid
/// parent-pointer chains; this function picks one and ignores the
/// other so a recipe's `has.actor: [["axe"]]` doesn't accidentally
/// match an axe sitting in the actor's DOWN-stack (and vice versa).
///
/// Implementation: scan cards whose `owner_id` is the soul (under
/// the new card-owner model, every inventory card carrying this soul
/// has `owner_id == soul_card_id`), build a parent → children map
/// keyed on `micro_location`, then BFS from `soul_card_id` with a
/// depth cap. Cards with `dead` or `slot_hold` set are excluded —
/// slot_held cards are claimed by an in-flight recipe and must not
/// be re-bound to another action's has-predicate concurrently. Cards
/// not in a chain state (`Free` / `OnHex`) are excluded — only
/// `OnRoot` (immediate stacked-on-root child) and `Slot` (parent-
/// pointer chain) qualify as equipment / action-stack positions.
pub fn soul_stack(
    ctx: &ReducerContext,
    soul_card_id: u32,
    direction: u8,
) -> Vec<Card> {
    if soul_card_id == 0 {
        return Vec::new();
    }
    // Build parent → children map from cards directly owned by this
    // soul. Iterating `owner_id` is btree-index-backed; the per-
    // card_id `cards::latest` lookups dedupe history rows naturally.
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut children_of: BTreeMap<u32, Vec<Card>> = BTreeMap::new();
    for row in ctx.db.cards().owner_id().filter(soul_card_id) {
        if !seen.insert(row.card_id) {
            continue;
        }
        let Some(latest) = cards::latest(ctx, row.card_id) else {
            continue;
        };
        // Skip dead and slot_held cards. Slot_held = "claimed by an
        // in-flight recipe" — re-binding it as a has-predicate match
        // for a *different* in-flight action would mean two actions
        // racing for the same card, with whichever finishes last
        // overwriting the other's release row.
        if latest.flags & (FLAG_DEAD | FLAG_SLOT_HOLD) != 0 {
            continue;
        }
        let (_, _, state) = unpack_micro_zone(latest.micro_zone);
        // Only chain states qualify — `Free` and `OnHex` cards aren't
        // stacked on anything in the equipment / action-stack sense.
        if !matches!(state, StackedState::OnRoot | StackedState::Slot) {
            continue;
        }
        // Filter by the requested stack branch.
        if micro_zone_direction(latest.micro_zone) != direction {
            continue;
        }
        if latest.micro_location == 0 {
            continue;
        }
        children_of
            .entry(latest.micro_location)
            .or_default()
            .push(latest);
    }

    // BFS from the soul. Depth-capped so a pathological cycle (which
    // shouldn't be possible but defensive coding) terminates.
    let mut out: Vec<Card> = Vec::new();
    let mut frontier: Vec<u32> = vec![soul_card_id];
    for _depth in 0..SOUL_STACK_MAX_DEPTH {
        let mut next: Vec<u32> = Vec::new();
        for parent in &frontier {
            if let Some(children) = children_of.remove(parent) {
                for child in children {
                    next.push(child.card_id);
                    out.push(child);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}

/// Bound card_ids for one stack direction (UP / DOWN) of `has` +
/// `reagents.has` predicate resolution. The lists are the
/// *concatenated* `has.X` ++ `reagents.has.X` order: `has.X.len()`
/// entries describe the non-consumed predicates, the remaining
/// `reagents.has.X.len()` entries describe the consumed ones.
///
/// Callers (action_completion) use `_consumed(len)` / `_predicate_only(len)`
/// to slice the appropriate tail / head.
#[derive(Debug, Clone, Default)]
pub struct RoleMatches {
    pub root: Vec<u32>,
    pub actor: Vec<u32>,
}

impl RoleMatches {
    /// Card ids that should be added to the consumed set on completion
    /// — the tail of `root` matching the length of
    /// `recipe.reagents.has.root`.
    pub fn root_consumed(&self, reagents_root_len: usize) -> &[u32] {
        let n = self.root.len();
        if reagents_root_len >= n {
            &self.root[..]
        } else {
            &self.root[n - reagents_root_len..]
        }
    }

    /// Card ids that should be added to the consumed set on completion
    /// — the tail of `actor` matching the length of
    /// `recipe.reagents.has.actor`.
    pub fn actor_consumed(&self, reagents_actor_len: usize) -> &[u32] {
        let n = self.actor.len();
        if reagents_actor_len >= n {
            &self.actor[..]
        } else {
            &self.actor[n - reagents_actor_len..]
        }
    }

    /// Predicate-only (non-consumed) head of `root` — entries from
    /// `has.root` that aren't tail-consumed by `reagents.has.root`.
    /// These cards carry slot_hold during the action but stay alive
    /// at completion; `action_completion::apply` adds them to its
    /// release set so the slot_hold clears at completion_secs.
    pub fn root_predicate_only(&self, reagents_root_len: usize) -> &[u32] {
        let n = self.root.len();
        if reagents_root_len >= n {
            &[]
        } else {
            &self.root[..n - reagents_root_len]
        }
    }

    /// Predicate-only (non-consumed) head of `actor` — see
    /// [`Self::root_predicate_only`] for semantics.
    pub fn actor_predicate_only(&self, reagents_actor_len: usize) -> &[u32] {
        let n = self.actor.len();
        if reagents_actor_len >= n {
            &[]
        } else {
            &self.actor[..n - reagents_actor_len]
        }
    }
}

/// Result of resolving a recipe's `has` / `has_below` predicate
/// blocks. `above` carries `has` + `reagents.has` (UP stack —
/// equipment); `below` carries `has_below` + `reagents.has_below`
/// (DOWN stack — action stack / debuffs).
#[derive(Debug, Clone, Default)]
pub struct HasMatches {
    pub above: RoleMatches,
    pub below: RoleMatches,
}

/// Resolve a recipe's `has` + `reagents.has` (UP stack) and
/// `has_below` + `reagents.has_below` (DOWN stack) predicates by
/// walking the relevant owner's soul stack and binding each entry
/// to a distinct alive card whose definition satisfies the entity.
/// On successful binding, stamps `slot_hold` at `time_secs` *only*
/// on cards that will be consumed (the `reagents.has*` tail) so a
/// concurrent action can't claim a dying card. Predicate-only
/// matches (the `has*` head) are checked at proposal time but
/// stay free for the rest of the action — they're still movable
/// by the player and other concurrent actions can predicate-only-
/// match them too.
///
/// Match ordering: entries are bound in DESCENDING entity specificity
/// (most specific first). This avoids "`Any` entries grab a specific
/// card before its specific entry can match it." E.g., `has.actor =
/// [Any, Card("axe")]` against stack `[axe]` — greedy by list order
/// would have `Any` grab `axe` and then `Card("axe")` would fail; the
/// specificity-sorted pass binds `Card("axe")` first, then `Any` has
/// no candidate and fails (correctly, since only one card exists).
///
/// `has.root` ++ `reagents.has.root` are concatenated and resolved
/// against `root_owner`'s soul UP-stack; same for actor. `has_below`
/// + `reagents.has_below` resolve the same way but against the
/// DOWN-stack. The lengths matter:
/// `HasMatches.above.root[..has.root.len()]` are non-consumed; the
/// rest are consumed via `reagents.has.root`. Same for the other
/// three lists.
///
/// `time_secs` is the wall-clock second the caller is stamping its
/// other hold writes at — `propose_action` passes `start_secs` (now),
/// `on_create::trigger` passes the card's `valid_at_time`,
/// `magnetic::commit_*` passes `commit_at`. The slot_hold row lands
/// at the same `valid_at` as the rest of the caller's claim writes,
/// keeping the "in-flight action's claim is visible from one moment"
/// discipline.
///
/// Returns `Err` with a descriptive message when:
/// - An owner has no soul card (world-owned cards can't wield
///   anything — recipe authors should design recipes that target
///   players, not world cards).
/// - Any required has-entity has no satisfying card in the stack.
///
/// Returns `Ok(HasMatches::default())` when the recipe declares no
/// has predicates (the common case for today's recipes).
#[allow(clippy::too_many_arguments)]
pub fn resolve_has(
    ctx: &ReducerContext,
    recipe_id: &str,
    has: &HasOps,
    reagents_has: &HasOps,
    has_below: &HasOps,
    reagents_has_below: &HasOps,
    root_soul_card_id: u32,
    actor_soul_card_id: u32,
    time_ms: u64,
) -> Result<HasMatches, String> {
    let above = bind_direction(
        ctx,
        recipe_id,
        has,
        reagents_has,
        root_soul_card_id,
        actor_soul_card_id,
        STACK_DIR_UP,
        "has",
    )?;
    let below = bind_direction(
        ctx,
        recipe_id,
        has_below,
        reagents_has_below,
        root_soul_card_id,
        actor_soul_card_id,
        STACK_DIR_DOWN,
        "has_below",
    )?;

    // Stamp `position_hold` on *every* matched card — predicate-only
    // and consumed alike — so the equipment can't be dragged off the
    // soul mid-action. `slot_hold` only goes on cards that will be
    // consumed at completion (the `reagents.has*` tail of each list);
    // predicate-only matches stay slot-free so other concurrent
    // actions can predicate-match them too (multiple actions can
    // "see" the same axe equipped). The consumed tail still needs
    // `slot_hold` so a concurrent action can't double-claim a dying
    // card.
    //
    // Done *after* both directions bind successfully so a partial-
    // binding failure (Err from the second `bind_direction`) doesn't
    // leave hold side-effects on cards bound by the first call.
    // Reducers roll back on Err so a stray write would be wiped
    // anyway, but keeping the writes within the success branch
    // matches the rest of `resolve_has`'s "no side effect on
    // failure" shape.
    //
    // Release happens at `action_completion::apply`'s
    // `completion_secs`: consumed cards die (their `release_mask`
    // clears every hold including `position_hold`); predicate-only
    // matches are added to the function's `release` set so their
    // `position_hold` clears via the same `release_mask`.
    for &card_id in above
        .root
        .iter()
        .chain(above.actor.iter())
        .chain(below.root.iter())
        .chain(below.actor.iter())
    {
        // Ref-counted with forward-prop: bumps the count at `time_ms`
        // and on every future-stamped row of this card. Lets two
        // recipes both has-predicate-match the same axe without one's
        // release prematurely transitioning the count to 0 while the
        // other still needs it.
        cards::acquire_position_hold(ctx, card_id, time_ms);
    }
    let consumed_iter = above
        .root_consumed(reagents_has.root.len())
        .iter()
        .chain(above.actor_consumed(reagents_has.actor.len()).iter())
        .chain(below.root_consumed(reagents_has_below.root.len()).iter())
        .chain(below.actor_consumed(reagents_has_below.actor.len()).iter());
    for &card_id in consumed_iter {
        cards::update_with_at(ctx, card_id, time_ms, |c| {
            c.flags |= FLAG_SLOT_HOLD;
        });
    }

    Ok(HasMatches { above, below })
}

/// Bind one direction's `has` + `reagents.has` predicates against
/// the relevant owners' soul stacks. Returns a [`RoleMatches`] with
/// the concatenated `has` ++ `reagents.has` list per role.
///
/// `direction` selects which soul branch the walker reads —
/// `STACK_DIR_UP` for `has`, `STACK_DIR_DOWN` for `has_below`.
/// `field_label` is `"has"` / `"has_below"` for error messages.
#[allow(clippy::too_many_arguments)]
fn bind_direction(
    ctx: &ReducerContext,
    recipe_id: &str,
    has: &HasOps,
    reagents_has: &HasOps,
    root_soul_card_id: u32,
    actor_soul_card_id: u32,
    direction: u8,
    field_label: &str,
) -> Result<RoleMatches, String> {
    let root_entries: Vec<&Entity> = has.root.iter().chain(reagents_has.root.iter()).collect();
    let actor_entries: Vec<&Entity> = has.actor.iter().chain(reagents_has.actor.iter()).collect();

    let root_matches = if root_entries.is_empty() {
        Vec::new()
    } else {
        bind_has_role(
            ctx,
            recipe_id,
            "root",
            &root_entries,
            root_soul_card_id,
            direction,
            field_label,
        )?
    };
    let actor_matches = if actor_entries.is_empty() {
        Vec::new()
    } else {
        bind_has_role(
            ctx,
            recipe_id,
            "actor",
            &actor_entries,
            actor_soul_card_id,
            direction,
            field_label,
        )?
    };

    Ok(RoleMatches {
        root: root_matches,
        actor: actor_matches,
    })
}

/// Bind one role's concatenated `has` entries to distinct card_ids in
/// the role-owner's soul stack along `direction`. Returns card_ids in
/// the *input* (caller-provided) order — i.e., aligned with the
/// `has.X` ++ `reagents.has.X` concatenation — so callers can slice
/// the consumed tail by length.
///
/// `direction` is `STACK_DIR_UP` for `has` / `STACK_DIR_DOWN` for
/// `has_below`. `field_label` is the JSON field name (`"has"` /
/// `"has_below"`) used in error messages.
///
/// Specificity-sorted greedy matching: walk entries in descending
/// specificity over candidate cards, claiming each card to its
/// highest-specificity entry. Failures surface as `Err` with which
/// entry / role couldn't be satisfied.
#[allow(clippy::too_many_arguments)]
fn bind_has_role(
    ctx: &ReducerContext,
    recipe_id: &str,
    role: &str,
    entries: &[&Entity],
    soul_card_id: u32,
    direction: u8,
    field_label: &str,
) -> Result<Vec<u32>, String> {
    if soul_card_id == 0 {
        return Err(format!(
            "recipe {recipe_id}: {field_label}.{role} requires a soul (world-owned role card has no soul stack)"
        ));
    }
    let stack = soul_stack(ctx, soul_card_id, direction);

    // Score each (entry_index, candidate_card_index) pair. Sort
    // pairs by descending specificity, then greedy-assign: walk
    // pairs in order; bind the entry to the candidate if neither
    // side is taken yet. `candidate_defs` is parallel to `stack`;
    // entries where `decode_definition` fails carry `None` and are
    // skipped by the scoring loop.
    let mut candidate_defs: Vec<Option<&CardDefinition>> = Vec::with_capacity(stack.len());
    for card in &stack {
        candidate_defs.push(
            decode_definition(card.packed_definition)
                .ok()
                .flatten(),
        );
    }

    let mut scored: Vec<(u32, usize, usize)> = Vec::new(); // (specificity, entry_idx, cand_idx)
    for (entry_idx, entry) in entries.iter().enumerate() {
        for (cand_idx, def_opt) in candidate_defs.iter().enumerate() {
            let Some(def) = def_opt else { continue };
            let s = entity_specificity(entry, def);
            if s > 0 {
                scored.push((s, entry_idx, cand_idx));
            }
        }
    }
    // Highest specificity first; tie-break by entry_idx then cand_idx
    // for deterministic ordering.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

    let mut entry_card: Vec<Option<u32>> = vec![None; entries.len()];
    let mut taken_cand: BTreeSet<usize> = BTreeSet::new();
    for (_score, entry_idx, cand_idx) in scored {
        if entry_card[entry_idx].is_some() {
            continue;
        }
        if taken_cand.contains(&cand_idx) {
            continue;
        }
        entry_card[entry_idx] = Some(stack[cand_idx].card_id);
        taken_cand.insert(cand_idx);
    }
    // Any unmatched entries → failure.
    let mut out = Vec::with_capacity(entries.len());
    for (i, slot) in entry_card.into_iter().enumerate() {
        match slot {
            Some(id) => out.push(id),
            None => {
                return Err(format!(
                    "recipe {recipe_id}: {field_label}.{role}[{i}] not satisfied — \
                     no matching card in soul {soul_card_id}'s stack"
                ));
            }
        }
    }
    Ok(out)
}

/// Candidate card definitions for has-specificity scoring. Four
/// pools because the role/direction combinations are independent:
/// `root_above` ≠ `actor_above` when the recipe has different
/// root/actor owners (e.g., a combat action where root is the
/// target and actor is the attacker — each walks a different
/// soul stack).
///
/// For on_create where root == actor, the same defs are passed for
/// both `root_*` and `actor_*` slots — that's expected and cheap.
#[derive(Debug, Default)]
pub struct HasCandidates<'a> {
    pub root_above: Vec<&'a CardDefinition>,
    pub actor_above: Vec<&'a CardDefinition>,
    pub root_below: Vec<&'a CardDefinition>,
    pub actor_below: Vec<&'a CardDefinition>,
}

/// Score how specifically a recipe's has-predicates match the
/// candidate stacks. Sums `entity_specificity` for the best-matching
/// candidate per entry, across all four role/direction combinations
/// (`has.root` × `root_above`, `has.actor` × `actor_above`,
/// `has_below.root` × `root_below`, `has_below.actor` × `actor_below`).
/// Both `has` and `reagents.has` entries contribute — they're
/// concatenated when resolved, and the bonus follows suit.
///
/// Used by `on_create::trigger`'s ranker as a tier-3 tiebreaker
/// after `(hex_spec, root_spec)` — recipes with more-specific has
/// constraints outrank looser ones that match the new card the
/// same way.
///
/// `feasible` companion: every has-entry's best score is positive.
/// `infeasible_entries` reports the first entry that scored 0 so
/// the ranker can filter recipes whose has-predicates have *no*
/// satisfying card in the relevant stack — that recipe simply
/// doesn't apply rather than picking and then failing at
/// `resolve_has` time.
pub fn has_specificity_bonus(recipe: &RecipeDef, candidates: &HasCandidates) -> u32 {
    let mut score: u32 = 0;
    score = score.saturating_add(role_specificity_sum(
        recipe.has.root.iter().chain(recipe.reagents.has.root.iter()),
        &candidates.root_above,
    ));
    score = score.saturating_add(role_specificity_sum(
        recipe.has.actor.iter().chain(recipe.reagents.has.actor.iter()),
        &candidates.actor_above,
    ));
    score = score.saturating_add(role_specificity_sum(
        recipe
            .has_below
            .root
            .iter()
            .chain(recipe.reagents.has_below.root.iter()),
        &candidates.root_below,
    ));
    score = score.saturating_add(role_specificity_sum(
        recipe
            .has_below
            .actor
            .iter()
            .chain(recipe.reagents.has_below.actor.iter()),
        &candidates.actor_below,
    ));
    score
}

/// True iff every has-entry on the recipe has at least one
/// non-zero-specificity candidate in the relevant pool. Used by
/// `on_create::trigger` to filter recipes whose has-predicates
/// can't possibly satisfy against the current soul stacks —
/// they're skipped at rank time rather than picked-then-failed.
///
/// Doesn't account for "two entries fighting for one card" — e.g.,
/// `has.actor = [axe, axe]` against `[axe]` passes this check
/// (both entries score on the one axe) but `resolve_has`'s greedy
/// assign would fail. Those cases still error at resolve time;
/// this filter just catches the no-candidate-at-all case.
pub fn has_predicates_feasible(recipe: &RecipeDef, candidates: &HasCandidates) -> bool {
    role_all_entries_feasible(
        recipe.has.root.iter().chain(recipe.reagents.has.root.iter()),
        &candidates.root_above,
    ) && role_all_entries_feasible(
        recipe.has.actor.iter().chain(recipe.reagents.has.actor.iter()),
        &candidates.actor_above,
    ) && role_all_entries_feasible(
        recipe
            .has_below
            .root
            .iter()
            .chain(recipe.reagents.has_below.root.iter()),
        &candidates.root_below,
    ) && role_all_entries_feasible(
        recipe
            .has_below
            .actor
            .iter()
            .chain(recipe.reagents.has_below.actor.iter()),
        &candidates.actor_below,
    )
}

fn role_specificity_sum<'a, I>(entries: I, pool: &[&CardDefinition]) -> u32
where
    I: IntoIterator<Item = &'a Entity>,
{
    let mut s: u32 = 0;
    for entry in entries {
        let best = pool
            .iter()
            .map(|d| entity_specificity(entry, d))
            .max()
            .unwrap_or(0);
        s = s.saturating_add(best);
    }
    s
}

fn role_all_entries_feasible<'a, I>(entries: I, pool: &[&CardDefinition]) -> bool
where
    I: IntoIterator<Item = &'a Entity>,
{
    for entry in entries {
        let best = pool
            .iter()
            .map(|d| entity_specificity(entry, d))
            .max()
            .unwrap_or(0);
        if best == 0 {
            return false;
        }
    }
    true
}
