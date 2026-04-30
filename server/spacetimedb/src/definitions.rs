// definitions.rs — compile-time-embedded card and recipe definitions.
//
// Card files are indexed as: key = (card_type << 8) | definition_id
// where definition_id is 1-based (cards[0] in JSON → definition_id 1).
// All current card definitions use category 0, so this key uniquely
// identifies any definition.

use std::collections::HashMap;
use std::sync::OnceLock;
use serde::Deserialize;
use serde_json::Value;

// ── Embedded JSON files ───────────────────────────────────────────────────────

const DISCIPLINE_JSON: &str = include_str!("../data/cards/discipline.json");
const FACULTY_JSON:    &str = include_str!("../data/cards/faculty.json");
const REQUISITES_JSON: &str = include_str!("../data/cards/requisites.json");
const REVERY_JSON:     &str = include_str!("../data/cards/revery.json");
const SOUL_JSON:       &str = include_str!("../data/cards/soul.json");
const TILE_JSON:       &str = include_str!("../data/cards/tile.json");
const RECIPES_JSON:    &str = include_str!("../data/recipes/basic.json");

// ── Raw deserialization shapes ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawCardFile {
    card_type: u8,
    cards: Vec<RawCardDef>,
}

#[derive(Debug, Deserialize)]
struct RawCardDef {
    id:           String,
    display_name: String,
    #[serde(default)]
    abilities: Vec<String>,
    #[serde(default)]
    aspects: HashMap<String, u8>,
}

#[derive(Debug, Deserialize)]
struct RawRecipe {
    id: String,
    #[serde(rename = "type")]
    recipe_type: Option<String>,
    #[serde(default)]
    duration: Value,
    tile:      Option<Value>,
    catalysts: Option<Value>,
    reagents:  Option<Value>,
    products:  Option<Value>,
}

// ── Parsed types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CardDef {
    pub id:           String,
    pub display_name: String,
    pub abilities:    Vec<String>,
    /// Aspect tags with additive values 1–3 indicating strength of association.
    pub aspects:  HashMap<String, u8>,
}

/// Recursive recipe entity tree — mirrors the TypeScript RecipeEntity type.
#[derive(Debug, Clone)]
pub enum Entity {
    Empty,
    Leaf { def_id: String, qty: u32 },
    And  { a: Box<Entity>, b: Box<Entity> },
    Or   { a: Box<Entity>, weights: [u32; 2], b: Box<Entity> },
}

/// How a recipe selects which cards to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeType {
    /// Acts on the top card of a stack.
    TopStack,
    /// Acts on the bottom card of a stack.
    BottomStack,
    /// Acts on both ends of a stack.
    BothStack,
    /// Queued automatically when a matching card is created.
    OnCreate,
    /// Triggered explicitly by player action.
    Explicit,
}

/// One entry in a conditional duration list.
#[derive(Debug, Clone)]
pub struct DurationCondition {
    pub duration:  u32,
    /// `Empty` means this entry always matches (catch-all).
    pub condition: Entity,
}

/// Recipe duration — either a fixed number of seconds or a prioritised
/// condition list evaluated against the card pool at action start.
#[derive(Debug, Clone)]
pub enum RecipeDuration {
    Fixed(u32),
    Conditional(Vec<DurationCondition>),
}

/// Where a product group's cards are placed when a recipe completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductTarget {
    /// Cards go into the owner's panel (owner_id from the action).
    Owner,
    /// Cards go into the root card's panel (card_id from the action).
    Root,
}

/// One group of products with a shared placement target.
#[derive(Debug, Clone)]
pub struct ProductGroup {
    pub target: ProductTarget,
    pub entity: Entity,
}

#[derive(Debug, Clone)]
pub struct RecipeDef {
    pub id:          String,
    /// 0-based wire index — the value stored in Action::recipe.
    pub index:       u16,
    pub recipe_type: RecipeType,
    pub duration:    RecipeDuration,
    pub tile:        Option<Entity>,
    pub catalysts:   Option<Entity>,
    pub reagents:    Option<Entity>,
    pub products:    Vec<ProductGroup>,
}

// ── Entity parsing ────────────────────────────────────────────────────────────

fn parse_entity(v: &Value) -> Entity {
    let arr = match v.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return Entity::Empty,
    };

    // Leaf: ["def_id", qty]
    if let Some(s) = arr[0].as_str() {
        let qty = arr.get(1).and_then(Value::as_u64).unwrap_or(1) as u32;
        return Entity::Leaf { def_id: s.to_owned(), qty };
    }

    // Compound: first element must be an array
    if !arr[0].is_array() { return Entity::Empty; }

    // OR form: three elements, third is a non-empty array
    if arr.len() == 3 {
        let c = &arr[2];
        if c.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            let weights = arr[1].as_array()
                .filter(|w| w.len() == 2)
                .and_then(|w| Some([
                    w[0].as_u64()? as u32,
                    w[1].as_u64()? as u32,
                ]))
                .unwrap_or([1, 1]);
            return Entity::Or {
                a: Box::new(parse_entity(&arr[0])),
                weights,
                b: Box::new(parse_entity(c)),
            };
        }
    }

    // AND form: [A, B] or [A, B, []]
    Entity::And {
        a: Box::new(parse_entity(&arr[0])),
        b: Box::new(arr.get(1).map(parse_entity).unwrap_or(Entity::Empty)),
    }
}

fn parse_duration(v: &Value) -> RecipeDuration {
    match v {
        Value::Number(n) => RecipeDuration::Fixed(n.as_u64().unwrap_or(0) as u32),
        Value::Array(arr) => {
            let conditions = arr.iter().filter_map(|entry| {
                let row = entry.as_array()?;
                let duration = row.first()?.as_u64()? as u32;
                let condition = match row.get(1) {
                    None => Entity::Empty,
                    Some(Value::Array(a)) if a.is_empty() => Entity::Empty,
                    Some(v) => parse_entity(v),
                };
                Some(DurationCondition { duration, condition })
            }).collect();
            RecipeDuration::Conditional(conditions)
        }
        _ => RecipeDuration::Fixed(0),
    }
}

// ── Registry ──────────────────────────────────────────────────────────────────

type CardKey = u16;

fn card_key(card_type: u8, definition_id: u8) -> CardKey {
    ((card_type as u16) << 8) | (definition_id as u16)
}

struct Registry {
    cards:      HashMap<CardKey, CardDef>,
    by_str_id:  HashMap<String, CardKey>,
    recipes:    Vec<RecipeDef>,
    recipe_ids: HashMap<String, u16>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn build_registry() -> Registry {
    const CARD_FILES: &[&str] = &[
        DISCIPLINE_JSON,
        FACULTY_JSON,
        REQUISITES_JSON,
        REVERY_JSON,
        SOUL_JSON,
        TILE_JSON,
    ];

    let mut cards     = HashMap::new();
    let mut by_str_id = HashMap::new();
    for json in CARD_FILES {
        let file: RawCardFile = match serde_json::from_str(json) {
            Ok(f)  => f,
            Err(e) => { log::warn!("definitions: failed to parse card file: {e}"); continue; }
        };
        for (i, raw) in file.cards.iter().enumerate() {
            let definition_id = (i + 1) as u8;
            let key = card_key(file.card_type, definition_id);
            by_str_id.insert(raw.id.clone(), key);
            cards.insert(key, CardDef {
                id:           raw.id.clone(),
                display_name: raw.display_name.clone(),
                abilities:    raw.abilities.clone(),
                aspects:      raw.aspects.clone(),
            });
        }
    }

    let raw_recipes: Vec<RawRecipe> = serde_json::from_str(RECIPES_JSON).unwrap_or_else(|e| {
        log::warn!("definitions: failed to parse recipes: {e}");
        Vec::new()
    });

    let mut recipes    = Vec::with_capacity(raw_recipes.len());
    let mut recipe_ids = HashMap::new();

    for (i, raw) in raw_recipes.into_iter().enumerate() {
        let index = i as u16;
        recipe_ids.insert(raw.id.clone(), index);
        let products = raw.products
            .as_ref()
            .and_then(Value::as_object)
            .map(|obj| {
                obj.iter().filter_map(|(key, val)| {
                    let target = match key.as_str() {
                        "owner" => ProductTarget::Owner,
                        "root"  => ProductTarget::Root,
                        _ => { log::warn!("definitions: unknown product target '{key}'"); return None; }
                    };
                    Some(ProductGroup { target, entity: parse_entity(val) })
                }).collect()
            })
            .unwrap_or_default();

        let recipe_type = match raw.recipe_type.as_deref() {
            Some("top_stack")    => RecipeType::TopStack,
            Some("bottom_stack") => RecipeType::BottomStack,
            Some("both_stack")   => RecipeType::BothStack,
            Some("on_create")    => RecipeType::OnCreate,
            Some("explicit")     => RecipeType::Explicit,
            Some(other) => {
                log::warn!("definitions: unknown recipe type '{other}', defaulting to OnCreate");
                RecipeType::OnCreate
            }
            None => RecipeType::OnCreate,
        };

        recipes.push(RecipeDef {
            id:          raw.id,
            index,
            recipe_type,
            duration:    parse_duration(&raw.duration),
            tile:        raw.tile.as_ref().map(parse_entity),
            catalysts:   raw.catalysts.as_ref().map(parse_entity),
            reagents:    raw.reagents.as_ref().map(parse_entity),
            products,
        });
    }

    Registry { cards, by_str_id, recipes, recipe_ids }
}

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(build_registry)
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Look up a card definition by card_type and 1-based definition_id.
/// Matches the packed_definition wire format (category 0 assumed).
pub fn get_card_def(card_type: u8, definition_id: u8) -> Option<&'static CardDef> {
    registry().cards.get(&card_key(card_type, definition_id))
}

/// Look up a recipe by its 0-based wire index (Action::recipe field).
pub fn get_recipe(index: u16) -> Option<&'static RecipeDef> {
    registry().recipes.get(index as usize)
}

/// Duration in seconds for the given recipe index, or `fallback` if not found.
/// For conditional durations, returns the first unconditional catch-all entry,
/// or `fallback` if none exists. Use `resolve_duration` for full evaluation.
pub fn recipe_duration(index: u16, fallback: u32) -> u32 {
    match get_recipe(index).map(|r| &r.duration) {
        Some(RecipeDuration::Fixed(n)) => *n,
        Some(RecipeDuration::Conditional(entries)) => entries.iter()
            .find(|e| matches!(e.condition, Entity::Empty))
            .map(|e| e.duration)
            .unwrap_or(fallback),
        None => fallback,
    }
}

/// Evaluate a conditional duration against a card pool, returning the first
/// matching entry's duration. Fixed durations ignore the pool entirely.
/// The pool maps card definition id → count and is not consumed.
pub fn resolve_duration(recipe: &RecipeDef, pool: &HashMap<String, u32>) -> u32 {
    match &recipe.duration {
        RecipeDuration::Fixed(n) => *n,
        RecipeDuration::Conditional(entries) => {
            for entry in entries {
                if matches!(entry.condition, Entity::Empty) {
                    return entry.duration;
                }
                let mut tmp = pool.clone();
                if match_entity(&entry.condition, &mut tmp) {
                    return entry.duration;
                }
            }
            0
        }
    }
}

/// Number of loaded recipes.
pub fn recipe_count() -> usize {
    registry().recipes.len()
}

/// Number of loaded card definitions.
pub fn card_def_count() -> usize {
    registry().cards.len()
}

/// Look up a card definition by its string id (e.g. "corpus", "log").
/// Returns (card_type, definition_id 1-based) for use with pack_definition.
pub fn find_def_by_str_id(id: &str) -> Option<(u8, u8)> {
    let &key = registry().by_str_id.get(id)?;
    Some(((key >> 8) as u8, (key & 0xFF) as u8))
}

/// Iterate all recipes whose type is OnCreate.
pub fn on_create_recipes() -> impl Iterator<Item = &'static RecipeDef> {
    registry().recipes.iter().filter(|r| r.recipe_type == RecipeType::OnCreate)
}

// ── Input matching ────────────────────────────────────────────────────────────
//
// Checks whether a pool of card definition ids satisfies an Entity requirement.
// Mutates the pool on success (cards consumed). Leaves it unmodified on failure.

fn match_entity(entity: &Entity, pool: &mut HashMap<String, u32>) -> bool {
    match entity {
        Entity::Empty => true,

        Entity::Leaf { def_id, qty } => {
            if def_id == "any" {
                let mut need = *qty;
                let mut taken: Vec<(String, u32)> = Vec::new();
                for (key, count) in pool.iter() {
                    if need == 0 { break; }
                    let take = (*count).min(need);
                    taken.push((key.clone(), take));
                    need -= take;
                }
                if need > 0 { return false; }
                for (key, take) in taken {
                    let v = pool.get_mut(&key).unwrap();
                    *v -= take;
                    if *v == 0 { pool.remove(&key); }
                }
                true
            } else {
                let have = pool.get(def_id).copied().unwrap_or(0);
                if have < *qty { return false; }
                let after = have - qty;
                if after == 0 { pool.remove(def_id); } else { pool.insert(def_id.clone(), after); }
                true
            }
        }

        Entity::And { a, b } => {
            let snapshot: HashMap<String, u32> = pool.clone();
            if !match_entity(a, pool) { return false; }
            if !match_entity(b, pool) { *pool = snapshot; return false; }
            true
        }

        Entity::Or { a, b, .. } => {
            let snapshot: HashMap<String, u32> = pool.clone();
            if match_entity(a, pool) { return true; }
            *pool = snapshot;
            match_entity(b, pool)
        }
    }
}

/// Returns true if the provided card definition id pool satisfies all input
/// requirements (tile, catalysts, reagents) of the given recipe.
/// `pool` maps definition id → count and is consumed by the check.
pub fn matches_inputs(recipe: &RecipeDef, pool: &mut HashMap<String, u32>) -> bool {
    if let Some(tile)      = &recipe.tile      { if !match_entity(tile,      pool) { return false; } }
    if let Some(catalysts) = &recipe.catalysts { if !match_entity(catalysts, pool) { return false; } }
    if let Some(reagents)  = &recipe.reagents  { if !match_entity(reagents,  pool) { return false; } }
    true
}

// ── Recipe priority scoring ───────────────────────────────────────────────────
//
// When multiple on_create recipes match the same card we pick the most
// specific one.  Specificity is the sum of per-leaf weights across catalysts
// and reagents (tile is structural and excluded from scoring).
//
// Leaf weight by match category (higher = more specific):
//   def id   1000  — key matches the card's own definition id exactly
//   aspect    100  — key is one of the card's aspect names
//   card type  10  — future: key names a card_type category
//   "any"       1  — wildcard

const WEIGHT_DEF_ID:    u32 = 1000;
const WEIGHT_ASPECT:    u32 = 100;
const WEIGHT_CARD_TYPE: u32 = 10;
const WEIGHT_ANY:       u32 = 1;

fn score_entity(entity: &Entity, def: &CardDef) -> u32 {
    match entity {
        Entity::Empty => 0,
        Entity::Leaf { def_id, qty } => {
            let w = if def_id == "any" {
                WEIGHT_ANY
            } else if def_id == &def.id {
                WEIGHT_DEF_ID
            } else if def.aspects.contains_key(def_id.as_str()) {
                WEIGHT_ASPECT
            } else {
                WEIGHT_CARD_TYPE
            };
            w * qty
        }
        Entity::And { a, b } => score_entity(a, def) + score_entity(b, def),
        // For OR, take whichever branch scores higher — we're asking "how
        // specific could this match be", not simulating a particular path.
        Entity::Or { a, b, .. } => score_entity(a, def).max(score_entity(b, def)),
    }
}

/// Specificity score for a recipe matched against a particular card definition.
/// Higher score wins when multiple on_create recipes match the same card.
pub fn score_recipe_for_card(recipe: &RecipeDef, def: &CardDef) -> u32 {
    let cat = recipe.catalysts.as_ref().map_or(0, |e| score_entity(e, def));
    let rea = recipe.reagents.as_ref().map_or(0, |e| score_entity(e, def));
    cat + rea
}
