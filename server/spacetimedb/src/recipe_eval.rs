//! Shared recipe-evaluation primitives.
//!
//! Currently: aspect-pool aggregation and pool-vs-entity boolean
//! evaluation, used by conditional-duration resolution in both
//! `propose_action` (pool = root + slots) and `on_create::trigger`
//! (pool = the new card).
//!
//! These are kept here rather than in `resonantdust_content::recipe_core`
//! because they're consumer-side: how the spacetime module *uses*
//! recipes against the cards table. If at some point the content crate
//! grows a public `entity_specificity` / `entity_satisfied_pool` API,
//! callers can migrate without behavior change.

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
