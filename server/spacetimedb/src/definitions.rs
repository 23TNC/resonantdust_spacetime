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

const DISCIPLINE_JSON: &str = include_str!("../static/cards/discipline.json");
const FACULTY_JSON:    &str = include_str!("../static/cards/faculty.json");
const REQUISITES_JSON: &str = include_str!("../static/cards/requisites.json");
const REVERY_JSON:     &str = include_str!("../static/cards/revery.json");
const SOUL_JSON:       &str = include_str!("../static/cards/soul.json");
const TILE_JSON:       &str = include_str!("../static/cards/tile.json");
const RECIPES_JSON:    &str = include_str!("../static/recipes/basic.json");

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
    id:          String,
    stack_craft: bool,
    #[serde(default)]
    duration: u32,
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
    /// Ordered list of abilities this card carries (e.g. "fleeting", "card_target").
    pub abilities: Vec<String>,
    /// Aspect tags with additive values 1–3 indicating strength of association.
    pub aspects: HashMap<String, u8>,
}

/// Recursive recipe entity tree — mirrors the TypeScript RecipeEntity type.
#[derive(Debug, Clone)]
pub enum Entity {
    Empty,
    Leaf { def_id: String, qty: u32 },
    And  { a: Box<Entity>, b: Box<Entity> },
    Or   { a: Box<Entity>, weights: [u32; 2], b: Box<Entity> },
}

#[derive(Debug, Clone)]
pub struct RecipeDef {
    pub id:          String,
    /// 0-based wire index — the value stored in Action::recipe.
    pub index:       u16,
    pub stack_craft: bool,
    pub duration:    u32,
    pub tile:        Option<Entity>,
    pub catalysts:   Option<Entity>,
    pub reagents:    Option<Entity>,
    pub products:    Option<Entity>,
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
        recipes.push(RecipeDef {
            id:          raw.id,
            index,
            stack_craft: raw.stack_craft,
            duration:    raw.duration,
            tile:        raw.tile.as_ref().map(parse_entity),
            catalysts:   raw.catalysts.as_ref().map(parse_entity),
            reagents:    raw.reagents.as_ref().map(parse_entity),
            products:    raw.products.as_ref().map(parse_entity),
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

/// Look up a recipe by its string id.
pub fn get_recipe_by_id(id: &str) -> Option<&'static RecipeDef> {
    let &idx = registry().recipe_ids.get(id)?;
    get_recipe(idx)
}

/// Duration in seconds for the given recipe index, or `fallback` if not found.
pub fn recipe_duration(index: u16, fallback: u32) -> u32 {
    get_recipe(index).map(|r| r.duration).unwrap_or(fallback)
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

/// Returns true if the card definition declares the named ability.
pub fn has_ability(card_type: u8, definition_id: u8, ability: &str) -> bool {
    get_card_def(card_type, definition_id)
        .map(|d| d.abilities.iter().any(|a| a == ability))
        .unwrap_or(false)
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
