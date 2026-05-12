use resonantdust_content::definition_core::{decode_definition, is_hex_type};
use resonantdust_content::recipe_core::{
    recipes_of_type, Duration, RecipeDef, RecipeType,
};
use spacetimedb::ReducerContext;

use crate::action_completion;
use crate::cards;
use crate::magnetic;
use crate::packed::valid_at_time;
use crate::recipe_eval::{aspect_pool, entity_satisfied_pool, entity_specificity};

// `cards/flags.json` bit positions we need to OR onto the new card row.
const FLAG_POSITION_HOLD: u32 = 1 << 0;
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
/// **Card-secs basis.** The trigger reads the card's `valid_at` rather
/// than `now`, so a future-stamped product (created via `create_at` at
/// `completion_secs` of an earlier action) gets its OnCreate scheduled
/// relative to its own `valid_at`, not relative to wall-clock at the
/// moment the product was committed. This means the with-holds row
/// goes on the create row's same `valid_at` (replacing it via
/// `write_at`'s find/delete/insert) and the completion lands at
/// `card.valid_at + duration`.
///
/// **Eligibility & ranking.** Mirrors `match_stack_recipe`'s shape: a
/// recipe is eligible iff every declared entity is satisfied —
/// `recipe.hex` requires the new card to be a hex-shaped type that
/// also satisfies the entity, and `recipe.root` matches any type
/// satisfying the entity. Among eligible recipes the highest
/// `(hex_spec, root_spec)` lexicographic tuple wins.
///
/// **Magnetic recipes** (`recipe.magnetic.is_some()`) take a separate
/// path: [`magnetic::install`] applies `set_start.hex` + the auto
/// `magnetic_hold`, inserts a `MagneticAction` row, and returns. The
/// non-magnetic completion-schedule code below does not run for them.
///
/// **Conditional durations** are rejected with `Err` for the
/// non-magnetic path (same policy as `propose_action`). The magnetic
/// path enforces the same restriction inside `magnetic::install`.
///
/// A no-match result is `Ok(())` — most card creations won't match any
/// OnCreate recipe and that's fine.
pub fn trigger(
    ctx: &ReducerContext,
    card_id: u32,
    caller_player_id: u32,
) -> Result<(), String> {
    let card = match cards::latest(ctx, card_id) {
        Some(c) => c,
        None => return Ok(()),
    };
    let def = match decode_definition(card.packed_definition)
        .map_err(|err| format!("on_create: decode_definition: {err}"))?
    {
        Some(d) => d,
        None => return Ok(()),
    };

    // Both `on_create/self` and `on_create/magnetic` recipes fire on
    // card creation — the latter are magnetic outers, which we still
    // surface here so the TODO skip-and-return branch below catches
    // them. The two lists are concatenated; the specificity comparison
    // below picks the best regardless of category.
    let self_candidates = recipes_of_type(RecipeType::OnCreate)
        .map_err(|err| format!("on_create: recipes_of_type: {err}"))?;
    let magnetic_candidates = recipes_of_type(RecipeType::OnCreateMagnetic)
        .map_err(|err| format!("on_create: recipes_of_type: {err}"))?;
    let candidates = self_candidates
        .into_iter()
        .chain(magnetic_candidates.into_iter());

    // Highest-specificity match wins. Tuple ordering: hex tier outranks
    // root tier outright (matches `match_stack_recipe`'s convention).
    let mut best: Option<((u32, u32), &'static RecipeDef)> = None;
    'recipes: for recipe in candidates {
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
        let score = (hex_spec, root_spec);
        if best.map_or(true, |(b, _)| score > b) {
            best = Some((score, recipe));
        }
    }

    let recipe = match best {
        Some((_, r)) => r,
        None => return Ok(()),
    };

    if recipe.magnetic.is_some() {
        // Magnetic path. `install` handles `set_start.hex` + auto
        // `magnetic_hold` on the anchor and inserts the MagneticAction
        // row; the non-magnetic completion-schedule code below is
        // intentionally skipped.
        return magnetic::install(ctx, recipe, card_id, caller_player_id);
    }

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

    let card_secs = valid_at_time(card.valid_at);
    let completion_secs = card_secs.saturating_add(duration_secs);

    // Set holds on the card row at `card_secs`. write_at finds the
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
    let position_hold = if recipe.hex.is_some() {
        FLAG_POSITION_HOLD
    } else {
        0
    };
    let set_start_root = recipe.set_start.root as u32;
    let set_start_hex = if recipe.hex.is_some() {
        recipe.set_start.hex as u32
    } else {
        0
    };
    cards::update_with_at(ctx, card_id, card_secs, |c| {
        c.flags |= FLAG_SLOT_HOLD | position_hold;
        c.flags |= set_start_root;
        c.flags |= set_start_hex;
    });

    // Per recipe_core's "OnCreate (root == actor)" convention, the new
    // card is always the chain root — and the actor, since OnCreate has
    // no slots. It's also `hex` when the recipe declares one.
    // `action_completion::apply`'s actor_id resolution picks `root`
    // first, so the new card receives `progress_style` on its
    // completion row.
    let root = card_id;
    let hex = if recipe.hex.is_some() { card_id } else { 0 };

    action_completion::apply(
        ctx,
        recipe,
        hex,
        root,
        /* slots */ &[],
        completion_secs,
        caller_player_id,
        /* hex_location */ None,
    )?;

    Ok(())
}

