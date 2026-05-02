// definitions.rs — compile-time-embedded card and recipe definitions.
//
// Card files are indexed as: key = (card_type << 8) | definition_id
// where definition_id is 1-based (cards[0] in JSON → definition_id 1).
// All current card definitions use category 0, so this key uniquely
// identifies any definition.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use serde::Deserialize;
use serde_json::Value;

// ── Embedded JSON files ───────────────────────────────────────────────────────

const CARD_TYPES_JSON: &str = include_str!("../data/card_types.json");
const DISCIPLINE_JSON: &str = include_str!("../data/cards/discipline.json");
const FACULTY_JSON:    &str = include_str!("../data/cards/faculty.json");
const REQUISITES_JSON: &str = include_str!("../data/cards/requisites.json");
const REVERY_JSON:     &str = include_str!("../data/cards/revery.json");
const SOUL_JSON:       &str = include_str!("../data/cards/soul.json");
const TILE_JSON:       &str = include_str!("../data/cards/tile.json");
const RECIPES_JSON:    &str = include_str!("../data/recipes/basic.json");

// ── Card type registry ────────────────────────────────────────────────────────
// Loaded from data/card_types.json.  Public API exposes a `CardTypeIds`
// struct of named u8 fields populated at first access.  The matching client
// values come from the same file via pixijs/src/definitions/CardTypes.ts.

const PUBLIC_MAX_ID: u8 = 3;

#[derive(Debug, Deserialize)]
struct RawCardTypesFile {
    types: HashMap<String, RawCardTypeEntry>,
}

#[derive(Debug, Deserialize)]
struct RawCardTypeEntry {
    id:         u8,
    visibility: String,
    /// Drawn-shape hint for the client; the server doesn't read this.
    #[serde(default)]
    #[allow(dead_code)]
    shape:      Option<String>,
}

#[derive(Debug, Clone)]
pub struct CardTypeIds {
    pub requisites:     u8,
    pub revery:         u8,
    pub discipline:     u8,
    pub faculty:        u8,
    pub soul:           u8,
    pub floor:          u8,
    pub tile_object:    u8,
    pub tile_decorator: u8,
}

static CARD_TYPE_IDS: OnceLock<CardTypeIds> = OnceLock::new();

fn load_card_type_registry() -> CardTypeIds {
    let file: RawCardTypesFile = serde_json::from_str(CARD_TYPES_JSON)
        .expect("card_types.json: failed to parse");

    let mut by_name: HashMap<String, u8> = HashMap::new();
    let mut by_id:   HashMap<u8, String> = HashMap::new();

    for (name, entry) in &file.types {
        let derived = if entry.id <= PUBLIC_MAX_ID { "public" } else { "private" };
        if entry.visibility != derived {
            panic!(
                "card_types.json: type '{}' (id {}) declares visibility '{}' but \
                 bit cutoff (id <= {}) implies '{}'",
                name, entry.id, entry.visibility, PUBLIC_MAX_ID, derived
            );
        }
        if let Some(other) = by_id.get(&entry.id) {
            panic!("card_types.json: id {} appears on both '{}' and '{}'",
                   entry.id, other, name);
        }
        by_id.insert(entry.id, name.clone());
        by_name.insert(name.clone(), entry.id);
    }

    let required = |key: &str| -> u8 {
        *by_name.get(key).unwrap_or_else(|| {
            panic!("card_types.json: required type '{}' not present", key)
        })
    };

    CardTypeIds {
        requisites:     required("requisites"),
        revery:         required("revery"),
        discipline:     required("discipline"),
        faculty:        required("faculty"),
        soul:           required("soul"),
        floor:          required("floor"),
        tile_object:    required("tile_object"),
        tile_decorator: required("tile_decorator"),
    }
}

/// Card type id constants, loaded from `data/card_types.json` on first access.
pub fn card_types() -> &'static CardTypeIds {
    CARD_TYPE_IDS.get_or_init(load_card_type_registry)
}

/// Determine the actor's chain index for a recipe.
///
/// - `on_create` / `explicit`: actor IS the root at chain[0].
/// - `top_stack`:
///   - With an explicit `root` precondition → actor at chain[1].
///   - Without a `root` precondition → actor at chain[0].
pub fn actor_index_for(recipe: &RecipeDef) -> usize {
    match recipe.recipe_type {
        RecipeType::TopStack => {
            if recipe.root.is_some() { 1 } else { 0 }
        }
        RecipeType::OnCreate | RecipeType::Explicit => 0,
    }
}

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
    /// Optional precondition on the root card (chain index 0).
    root:     Option<Value>,
    /// Ordered slot entities; one per chain position outward from the actor.
    /// May be empty (on_create recipes typically have no positional slots).
    #[serde(default)]
    slots:    Vec<Value>,
    /// Chain indexes that are consumed at recipe completion.
    /// 0 = root, 1+ = slot[i-1] (chain index = slot index + actor_index).
    #[serde(default)]
    reagents: Vec<u8>,
    products: Option<Value>,
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
    /// Walks the up branch from actor outward.
    TopStack,
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
///
/// Per `RECIPE_REDESIGN.md` §6.  The world-placement variants are reserved
/// here but their generators are stubs — wiring them requires a placement
/// rule for "where in the hex / which empty cell."  Defer to the floor /
/// zone recipe phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductTarget {
    /// Action owner's inventory panel.  (Old key: "owner".)
    ActorPanel,
    /// Root card owner's inventory panel.  (Old key: "root".)
    RootPanel,
    /// Action owner soul's world hex.  Reserved.
    ActorWorld,
    /// Root owner soul's world hex.  Reserved.
    RootOwnerWorld,
    /// Root card's own world hex.  Reserved.
    RootWorld,
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
    /// Optional precondition on the root card (chain index 0).
    pub root:        Option<Entity>,
    /// Ordered slot entities; one per chain position outward from the actor.
    /// `slots[i]` matches against `chain[actor_index + i]`.
    pub slots:       Vec<Entity>,
    /// Chain indexes that get consumed at completion.  0 = root, 1+ = chain
    /// position (offset by actor_index for slot positions).  Stored sorted
    /// and deduped at parse time.
    pub reagents:    Vec<u8>,
    pub products:    Vec<ProductGroup>,
}

// ── Entity parsing ────────────────────────────────────────────────────────────

fn parse_entity(v: &Value) -> Entity {
    // Bare string: "defId" → Leaf with qty=1.  Allows strings as OR branches.
    if let Some(s) = v.as_str() {
        return Entity::Leaf { def_id: s.to_owned(), qty: 1 };
    }

    let arr = match v.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return Entity::Empty,
    };

    // OR form: [A, [wa, wb], C] — middle element is a pure-number array (including []).
    // Detected before the string-leaf check so string branches ("log", "vigor") work.
    if arr.len() == 3 {
        if let Some(mid) = arr[1].as_array() {
            if mid.iter().all(|w| w.is_number()) {
                let weights = if mid.len() == 2 {
                    [mid[0].as_u64().unwrap_or(1) as u32, mid[1].as_u64().unwrap_or(1) as u32]
                } else {
                    [1, 1]
                };
                return Entity::Or {
                    a: Box::new(parse_entity(&arr[0])),
                    weights,
                    b: Box::new(parse_entity(&arr[2])),
                };
            }
        }
    }

    // Leaf: ["defId"] or ["defId", qty]
    if let Some(s) = arr[0].as_str() {
        let qty = arr.get(1).and_then(Value::as_u64).unwrap_or(1) as u32;
        return Entity::Leaf { def_id: s.to_owned(), qty };
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
                // Bare number: unconditional catch-all.
                if let Some(n) = entry.as_u64() {
                    return Some(DurationCondition { duration: n as u32, condition: Entity::Empty });
                }
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
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn build_registry() -> Registry {
    // Force the card-type registry to load + validate alongside.
    let _ = card_types();

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

    let mut recipes = Vec::with_capacity(raw_recipes.len());

    for (i, raw) in raw_recipes.into_iter().enumerate() {
        let index = i as u16;
        let products = raw.products
            .as_ref()
            .and_then(Value::as_object)
            .map(|obj| {
                obj.iter().filter_map(|(key, val)| {
                    let target = match key.as_str() {
                        // Canonical names per RECIPE_REDESIGN.md §6.
                        "actor_panel"      => ProductTarget::ActorPanel,
                        "root_panel"       => ProductTarget::RootPanel,
                        "actor_world"      => ProductTarget::ActorWorld,
                        "root_owner_world" => ProductTarget::RootOwnerWorld,
                        "root_world"       => ProductTarget::RootWorld,
                        // Legacy aliases — accept and warn-on-removal in a later sweep.
                        "owner"            => ProductTarget::ActorPanel,
                        "root"             => ProductTarget::RootPanel,
                        "world"            => ProductTarget::RootWorld,
                        _ => { log::warn!("definitions: unknown product target '{key}'"); return None; }
                    };
                    Some(ProductGroup { target, entity: parse_entity(val) })
                }).collect()
            })
            .unwrap_or_default();

        let recipe_type = match raw.recipe_type.as_deref() {
            Some("top_stack") => RecipeType::TopStack,
            Some("on_create") => RecipeType::OnCreate,
            Some("explicit")  => RecipeType::Explicit,
            Some(other) => {
                log::warn!("definitions: unknown recipe type '{other}', defaulting to OnCreate");
                RecipeType::OnCreate
            }
            None => RecipeType::OnCreate,
        };

        // Parse slots and validate reagent indexes against them.
        let slots: Vec<Entity> = raw.slots.iter().map(parse_entity).collect();
        let max_chain_idx: u8 = (slots.len() as u8).saturating_add(1);  // root + slots
        let mut reagents = raw.reagents;
        reagents.sort();
        reagents.dedup();
        for &i in &reagents {
            if i >= max_chain_idx {
                log::warn!(
                    "definitions: recipe '{}' reagent index {} out of range (max {})",
                    raw.id, i, max_chain_idx.saturating_sub(1)
                );
            }
        }

        recipes.push(RecipeDef {
            id:          raw.id,
            index,
            recipe_type,
            duration:    parse_duration(&raw.duration),
            root:        raw.root.as_ref().map(parse_entity),
            slots,
            reagents,
            products,
        });
    }

    Registry { cards, by_str_id, recipes }
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
                if match_entity_pool(&entry.condition, &mut tmp) {
                    return entry.duration;
                }
            }
            0
        }
    }
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

/// Iterate all recipes whose type is TopStack.
pub fn top_stack_recipes() -> impl Iterator<Item = &'static RecipeDef> {
    registry().recipes.iter().filter(|r| r.recipe_type == RecipeType::TopStack)
}

// ── Duration condition matching ───────────────────────────────────────────────
//
// Used only for resolve_duration: checks whether a single card's aspect pool
// satisfies an entity condition.  Aspect quantities represent the card's aspect
// strength (value), not card counts.  Pool is mutated on success.

fn match_entity_pool(entity: &Entity, pool: &mut HashMap<String, u32>) -> bool {
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
            let snapshot = pool.clone();
            if !match_entity_pool(a, pool) { return false; }
            if !match_entity_pool(b, pool) { *pool = snapshot; return false; }
            true
        }

        Entity::Or { a, b, .. } => {
            let snapshot = pool.clone();
            if match_entity_pool(a, pool) { return true; }
            *pool = snapshot;
            match_entity_pool(b, pool)
        }
    }
}

// ── Adjacency matching ────────────────────────────────────────────────────────
//
// Recipe matching walks a chain (root + outward branch) and checks whether
// the recipe's `slots` array fits the chain at the actor's position.
// Adjacency means each slot maps to a fixed chain position; no permutation
// search is needed.
//
// Slot weights (higher = more specific) — used to pick between competing
// recipes that match at the same chain position:
//
//   exact def id  4  — card's definition id matches the slot's def_id
//   aspect name   3  — card has the slot's def_id as an aspect key
//   "any"         1  — wildcard accepts any card

pub const WEIGHT_DEF_ID: u32 = 4;
pub const WEIGHT_ASPECT: u32 = 3;
pub const WEIGHT_ANY:    u32 = 1;

/// Score a leaf-id against a card definition.  Returns None if it doesn't fit.
fn score_leaf(def: &CardDef, leaf_id: &str) -> Option<u32> {
    if leaf_id == "any"           { return Some(WEIGHT_ANY); }
    if def.id == leaf_id          { return Some(WEIGHT_DEF_ID); }
    if def.aspects.contains_key(leaf_id) { return Some(WEIGHT_ASPECT); }
    None
}

/// Match a single entity (slot or root condition) against a card.
/// Returns the best weight on success, or None.
///
/// Recursive: AND requires both branches to satisfy; OR returns the
/// best-matching branch.  Aspect leaves with an explicit qty check the
/// card's aspect value; `qty=1` (the default for bare strings) is a
/// presence-only check.
fn match_entity_card(entity: &Entity, def: &CardDef) -> Option<u32> {
    match entity {
        Entity::Empty => Some(0),

        Entity::Leaf { def_id, qty } => {
            // Aspect-with-qty: card's aspect value must meet the threshold.
            if *qty > 1 {
                let val = def.aspects.get(def_id).copied().unwrap_or(0);
                if (val as u32) < *qty { return None; }
                return Some(WEIGHT_ASPECT);
            }
            score_leaf(def, def_id)
        }

        Entity::And { a, b } => {
            let wa = match_entity_card(a, def)?;
            let wb = match_entity_card(b, def)?;
            Some(wa + wb)
        }

        Entity::Or { a, b, .. } => {
            let wa = match_entity_card(a, def);
            let wb = match_entity_card(b, def);
            match (wa, wb) {
                (Some(x), Some(y)) => Some(x.max(y)),
                (Some(x), None)    => Some(x),
                (None,    Some(y)) => Some(y),
                (None,    None)    => None,
            }
        }
    }
}

/// Result of a successful adjacency match.
pub struct RecipeMatchResult {
    /// Sum of per-slot match weights.  Higher = more specific assignment.
    pub weight: u32,
    /// card_ids of cards at the chain positions named by `recipe.reagents`.
    /// These get deleted at completion.  Order matches `recipe.reagents`.
    pub reagent_card_ids: Vec<u32>,
}

/// Try to match a recipe at a specific chain position.
///
/// `chain` is the ordered list of cards from root (index 0) outward up
/// the stack.  `actor_index` is the chain position the recipe's `slots[0]`
/// should be checked against.  For top_stack recipes, `actor_index` is
/// typically 1 (just past the root); for on_create recipes the trigger is
/// treated as the root and slots is empty so `actor_index` is irrelevant.
/// `held` is the set of card_ids currently claimed by some running action
/// — sourced from `CardHold` rows, never from card flags.
///
/// Returns None if any of these are true:
/// - `recipe.root` precondition fails against `chain[0]`.
/// - The chain is too short to contain all of `recipe.slots`.
/// - Any slot doesn't match the card at its target chain position.
/// - Any card in the matched window appears in `held`.
pub fn try_match_recipe_at(
    recipe:      &RecipeDef,
    chain:       &[(u32, &CardDef)],
    actor_index: usize,
    held:        &HashSet<u32>,
) -> Option<RecipeMatchResult> {
    if chain.is_empty() { return None; }

    // Root precondition.
    let mut weight = 0;
    if let Some(root_entity) = &recipe.root {
        weight += match_entity_card(root_entity, chain[0].1)?;
    }

    // Slots must fit within the chain past actor_index.
    let end = actor_index + recipe.slots.len();
    if end > chain.len() { return None; }

    // Per-slot positional check; bail on first mismatch or held card.
    for (slot_i, slot_entity) in recipe.slots.iter().enumerate() {
        let chain_pos = actor_index + slot_i;
        let (card_id, card_def) = chain[chain_pos];
        if held.contains(&card_id) { return None; }
        weight += match_entity_card(slot_entity, card_def)?;
    }

    // Build reagent card_id list in the order the recipe specified.
    let mut reagent_card_ids = Vec::with_capacity(recipe.reagents.len());
    for &chain_idx in &recipe.reagents {
        let i = chain_idx as usize;
        if i >= chain.len() {
            // Out-of-range reagent index.  Defensive — parse-time validation
            // should already have warned about this.
            return None;
        }
        reagent_card_ids.push(chain[i].0);
    }

    Some(RecipeMatchResult { weight, reagent_card_ids })
}

