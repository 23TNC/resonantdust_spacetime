use resonantdust_content::definition_core::{decode_definition, is_hex_type, CardDefinition};
use resonantdust_content::recipe_core::{
    recipes_of_type, Duration, RecipeDef, RecipeType,
};
use spacetimedb::{log, ReducerContext};

use crate::action_completion;
use crate::cards;
// Magnetic state is stamped on the card row via definition-flag
// inheritance in `cards::create_at`; there is no separate on_create
// recipe path for magnetic cards. See
// [docs/MAGNETIC_REWRITE.md](../../../../../docs/MAGNETIC_REWRITE.md).
use crate::packed::{valid_at_time, STACK_DIR_DOWN, STACK_DIR_UP};
use crate::recipe_eval::{
    aspect_pool, entity_satisfied_pool, entity_specificity, has_predicates_feasible,
    has_specificity_bonus, soul_stack, HasCandidates,
};

// `cards/flags.json` bit positions we need to OR onto the new card row.
const FLAG_SLOT_HOLD: u32 = 1 << 5;

/// Run `OnCreate` recipe matching against a freshly-created card and, if
/// a non-magnetic recipe matches, install it: set position/slot holds
/// on the card row and schedule the recipe's completion at
/// `card.valid_at + recipe.duration` via [`action_completion::apply`].
///
/// **When to call.** Every code path that creates a brand-new card —
/// `utilities::add_card` and the product loop in
/// `action_completion::apply` today; any future creation site too. Each
/// new card_id should fire this exactly once.
///
/// **`time_secs` parameter.** Callers pass the `valid_at` of the
/// card's create row — i.e., the time at which the card "exists" in
/// the cards-table timeline. For immediate creates
/// (`utilities::add_card`, `players::spawn_soul_for`, `bootstrap`)
/// this is `now_secs(ctx)`; for action products created via
/// `cards::create_at(...completion_secs)` it's `completion_secs`. The
/// fetch uses `cards::prior_at(..., time_secs)` rather than
/// `cards::latest(...)` so a future-stamped product is visible to
/// the trigger (a `latest` call would filter it out by the
/// `valid_at <= now` bound and silently return `None`, which is the
/// bug this parameter exists to fix).
///
/// **Card-secs basis.** Downstream timing (with-holds row, completion
/// schedule) is anchored to `card.valid_at` returned by `prior_at`.
/// For an immediate create that equals `now`; for a future-stamped
/// create it equals `completion_secs` of the parent action — so the
/// OnCreate completion lands at `(parent completion) + duration`,
/// not at `now + duration`.
///
/// **Eligibility & ranking.** Mirrors `match_stack_recipe`'s shape: a
/// recipe is eligible iff every declared entity is satisfied —
/// `recipe.hex` requires the new card to be a hex-shaped type that
/// also satisfies the entity, and `recipe.root` matches any type
/// satisfying the entity. Among eligible recipes the highest
/// `(hex_spec, root_spec)` lexicographic tuple wins.
///
/// **Conditional durations** are rejected with `Err` (same policy as
/// `propose_action`).
///
/// A no-match result is `Ok(())` — most card creations won't match any
/// OnCreate recipe and that's fine.
pub fn trigger(
    ctx: &ReducerContext,
    card_id: u32,
    caller_player_id: u32,
    time_ms: u64,
) -> Result<(), String> {
    let card = match cards::prior_at(ctx, card_id, time_ms) {
        Some(c) => c,
        None => return Ok(()),
    };
    let def = match decode_definition(card.packed_definition)
        .map_err(|err| format!("on_create: decode_definition: {err}"))?
    {
        Some(d) => d,
        None => return Ok(()),
    };

    let recipe_candidates = recipes_of_type(RecipeType::OnCreate)
        .map_err(|err| format!("on_create: recipes_of_type: {err}"))?
        .into_iter();

    // Walk the owner's soul UP / DOWN stacks once so the has-specificity
    // tiebreaker can score recipes against them without re-walking
    // per-candidate. For on_create, root == actor (same owner), so the
    // same defs feed `root_*` and `actor_*` slots of `HasCandidates`.
    //
    // Under the post-flag-20 card-owner model, walk up from this card
    // to find the soul that contains it. World-owned cards return
    // `None` here → empty stacks → recipes with `has` predicates are
    // filtered out via `has_predicates_feasible` below, which is the
    // correct behavior (a tree can't carry an axe).
    let soul_id = crate::cards::owning_soul(ctx, card_id).unwrap_or(0);
    let stack_above = soul_stack(ctx, soul_id, STACK_DIR_UP);
    let stack_below = soul_stack(ctx, soul_id, STACK_DIR_DOWN);
    let defs_above: Vec<&CardDefinition> = stack_above
        .iter()
        .filter_map(|c| decode_definition(c.packed_definition).ok().flatten())
        .collect();
    let defs_below: Vec<&CardDefinition> = stack_below
        .iter()
        .filter_map(|c| decode_definition(c.packed_definition).ok().flatten())
        .collect();
    let has_candidates = HasCandidates {
        root_above: defs_above.clone(),
        actor_above: defs_above,
        root_below: defs_below.clone(),
        actor_below: defs_below,
    };

    // Highest-specificity match wins. Tuple ordering:
    //   (hex_spec, root_spec, has_spec) lexicographic.
    // hex tier outranks root tier outranks has tier (matches
    // `match_stack_recipe`'s convention extended to has). `has_spec`
    // sums best-match specificity across `has` + `has_below` +
    // their `reagents.*` companions; only counted on recipes whose
    // has-predicates are feasible against the current soul stacks
    // (`has_predicates_feasible` skips otherwise).
    let mut best: Option<((u32, u32, u32), &'static RecipeDef)> = None;
    'recipes: for recipe in recipe_candidates {
        let hex_spec = match &recipe.hex {
            None => 0,
            Some(entity) => {
                let is_hex = is_hex_type(def.card_type)
                    .map_err(|err| format!("on_create: is_hex_type: {err}"))?;
                if !is_hex {
                    continue 'recipes;
                }
                let s = entity_specificity(entity, def);
                if s == 0 {
                    continue 'recipes;
                }
                s
            }
        };
        let root_spec = match &recipe.root {
            None => 0,
            Some(entity) => {
                let s = entity_specificity(entity, def);
                if s == 0 {
                    continue 'recipes;
                }
                s
            }
        };
        // Skip recipes whose has-predicates can't possibly satisfy —
        // doesn't apply to this card-creation context, so it
        // shouldn't pre-empt a feasible lower-tier match.
        if !has_predicates_feasible(recipe, &has_candidates) {
            continue 'recipes;
        }
        let has_spec = has_specificity_bonus(recipe, &has_candidates);
        let score = (hex_spec, root_spec, has_spec);
        if best.map_or(true, |(b, _)| score > b) {
            best = Some((score, recipe));
        }
    }

    let recipe = match best {
        Some((_, r)) => r,
        None => return Ok(()),
    };

    // Pre-rewrite there was a magnetic-install branch here. Magnetic
    // install now happens in `cards::create_at` via definition-flag
    // inheritance + the `lifecycle_pending` hook; no separate
    // recipe-driven path.

    let duration_secs = match &recipe.duration {
        Some(Duration::Fixed(s)) => *s,
        Some(Duration::Conditional { cases, fallback }) => {
            // OnCreate's aspect pool is just the new card's own aspects —
            // there are no slots and the new card is the only chain
            // member. Cases evaluate in declaration order; first match
            // wins; falls back to the trailing default.
            let pool = aspect_pool(std::iter::once(def));
            let mut hit: Option<u32> = None;
            for (secs, entity) in cases {
                if entity_satisfied_pool(entity, &pool).map_err(|err| {
                    format!(
                        "on_create: recipe {} ({}): conditional duration entity: {err}",
                        recipe.index, recipe.id
                    )
                })? {
                    hit = Some(*secs);
                    break;
                }
            }
            hit.unwrap_or(*fallback)
        }
        None => {
            return Err(format!(
                "on_create: recipe {} ({}): missing duration",
                recipe.index, recipe.id
            ));
        }
    };

    let card_ms = valid_at_time(card.valid_at);
    // Recipe `duration` is authored in seconds (JSON int); convert to ms.
    let completion_ms = card_ms.saturating_add((duration_secs as u64) * 1_000);

    // Set holds on the card row at `card_ms`. write_at finds the
    // existing row at that pk, deletes, re-inserts — so the card's
    // first visible row to the client is the held version, not a
    // brief flagless flicker.
    //
    // OnCreate also applies the recipe's `set_start.root` (always —
    // the new card is conceptually root per recipe_core's
    // "OnCreate root == actor") and `set_start.hex` (when the recipe
    // declared a hex entity). `set_start.slot` is meaningful only for
    // recipes that have slots (i.e. magnetic recipes via inner-recipe
    // slot fills) and so is ignored here.
    // `position_hold` is only meaningful when the new card is spatially
    // pinned by the recipe — which today means the recipe declared
    // `hex` (so the new card is both root and hex, pinned to itself).
    // A pure root-only OnCreate like `fatigue` doesn't pin the card
    // anywhere: it just sits where it was spawned and the player should
    // still be able to move it around mid-action. Setting position_hold
    // there would block drag for no reason. `slot_hold` always applies —
    // it's the "this card is participating in an action" signal that
    // the recipe-eligibility / drop checks key on.
    //
    // Same gating principle as `actions.rs`'s slot[0]/root spatial
    // writes (force_position only when there's something to pin to).
    let take_position_hold = recipe.hex.is_some();
    // `set_start.root` is always applied; `set_start.hex` only fires
    // when the recipe declared a hex entity. Author's set_start runs
    // *last* so it can override the auto-set holds above (e.g.
    // `set_start.root.slot_hold = false` releases the auto slot_hold).
    let set_start_root = recipe.set_start.root;
    let set_start_hex = if recipe.hex.is_some() {
        recipe.set_start.hex
    } else {
        resonantdust_content::recipe_core::FlagOps::default()
    };
    cards::update_with_at(ctx, card_id, card_ms, |c| {
        c.flags |= FLAG_SLOT_HOLD;
        if take_position_hold {
            c.flags = cards::increment_position_hold_count(c.flags);
        }
        c.flags = set_start_root.apply(c.flags);
        c.flags = set_start_hex.apply(c.flags);
    });
    if take_position_hold {
        // Forward-prop the count bump to any future-stamped rows of
        // this card (death rows, etc.). For a fresh-created card
        // there usually aren't any, but the helper is cheap.
        cards::propagate_position_hold_forward(ctx, card_id, card_ms, true);
    }

    // Per recipe_core's "OnCreate (root == actor)" convention, the new
    // card is always the chain root — and the actor, since OnCreate has
    // no slots. It's also `hex` when the recipe declares one.
    // `action_completion::apply`'s actor_id resolution picks `root`
    // first, so the new card receives `progress_style` on its
    // completion row.
    let root = card_id;
    let hex = if recipe.hex.is_some() { card_id } else { 0 };

    // OnCreate: root == actor (no slots), both share the same soul
    // (the soul that ultimately contains this card). Resolve any
    // `has` / `reagents.has` (UP stack) and `has_below` /
    // `reagents.has_below` (DOWN stack) predicates against that
    // soul's stack.
    //
    // A `resolve_has` error here would otherwise abort the entire
    // card-creation site (add_card / bootstrap / action_completion
    // products / world_gen). on_create is best-effort — if the
    // best-ranked recipe's has-predicates fail to greedy-bind
    // (e.g., two identical entries with only one matching card —
    // the ranker's `has_predicates_feasible` filter only catches
    // the no-candidate case), we log and skip the install rather
    // than fail the whole reducer.
    let has_matches = match crate::recipe_eval::resolve_has(
        ctx,
        &recipe.id,
        &recipe.has,
        &recipe.reagents.has,
        &recipe.has_below,
        &recipe.reagents.has_below,
        soul_id,
        soul_id,
        card_ms,
    ) {
        Ok(m) => m,
        Err(e) => {
            log::warn!(
                "on_create: card {card_id}: recipe {} ({}) has-predicates didn't bind: {e}",
                recipe.index,
                recipe.id
            );
            return Ok(());
        }
    };

    action_completion::apply(
        ctx,
        recipe,
        hex,
        root,
        /* slots */ &[],
        completion_ms,
        caller_player_id,
        /* hex_location */ None,
        has_matches,
    )?;

    Ok(())
}

