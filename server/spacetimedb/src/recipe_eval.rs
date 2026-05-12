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
//!
//! These are kept here rather than in `resonantdust_content::recipe_core`
//! because they're consumer-side: how the spacetime module *uses*
//! recipes against the cards table.

use std::collections::BTreeMap;

use resonantdust_content::definition_core::{AspectId, CardDefinition};
use resonantdust_content::recipe_core::Entity;

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
        Entity::And(a, b) => {
            Ok(entity_satisfied_pool(a, pool)? && entity_satisfied_pool(b, pool)?)
        }
        Entity::Or(a, b) | Entity::WeightedOr { a, b, .. } => {
            Ok(entity_satisfied_pool(a, pool)? || entity_satisfied_pool(b, pool)?)
        }
        Entity::Card(_) | Entity::Type(_) => Err(
            "conditional duration entities can only reference aspects, \
             not specific cards or types"
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
    match entity {
        Entity::Card(key) => {
            if &def.key == key {
                4
            } else {
                0
            }
        }
        Entity::Aspect(aspect, min) => {
            let val = def
                .aspects
                .iter()
                .find_map(|(a, v)| (a == aspect).then_some(*v))
                .unwrap_or(0);
            if val >= *min {
                3
            } else {
                0
            }
        }
        Entity::Type(type_id) => {
            if def.card_type == *type_id {
                2
            } else {
                0
            }
        }
        Entity::Any => 1,
        Entity::And(a, b) => {
            let sa = entity_specificity(a, def);
            let sb = entity_specificity(b, def);
            if sa > 0 && sb > 0 {
                sa + sb
            } else {
                0
            }
        }
        Entity::Or(a, b) | Entity::WeightedOr { a, b, .. } => {
            entity_specificity(a, def).max(entity_specificity(b, def))
        }
    }
}
