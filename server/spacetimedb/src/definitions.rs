//! Card and aspect definition registries.
//!
//! Decodes a `packed_definition` (`[card_type:u4][card_category:u4][definition_id:u8]`)
//! into a `CardDefinition` carrying display name, color style, and aspect
//! list. Used by the action machinery, which evaluates recipes against the
//! aspects of the cards in a stack.
//!
//! # Loading
//!
//! Source data lives in `<repo>/data/`:
//!
//! - `card_types.json` — registry of `card_type` and `card_category` ids.
//! - `aspects.json` — grouped aspect catalog. Aspects are 1-indexed in
//!   JSON insertion order across all groups (id 0 reserved as `ASPECT_NONE`).
//! - `cards/*.json` — per-file arrays of buckets, each bucket pinning a
//!   `card_type` + `category` and listing its cards as
//!   `{ key: [name, [c0, c1, c2], [[aspect_name, value], ...]] }`.
//!
//! Card aspect names are translated to `AspectId`s at registry-build time;
//! `CardDefinition.aspects` carries `(AspectId, i32)` pairs for fast runtime
//! aggregation.
//!
//! `definition_id` is the 1-based position of a card within its bucket; 0
//! reserved as sentinel. `serde_json`'s `preserve_order` feature is enabled
//! so insertion order matches the JSON file.
//!
//! # Failure mode
//!
//! Each registry is built lazily on first access and stored in an
//! `OnceLock<Result<Registry, String>>`. If a build fails (malformed JSON,
//! unknown aspect referenced from card data, id out of range, etc.) the
//! error is **stored** in the cell — every subsequent accessor returns the
//! same `Err(_)` rather than re-running the build and re-paying the failure.
//! This avoids the panic-loop pattern an earlier version had.
//!
//! # Paths
//!
//! Files are embedded with `include_str!` at compile time. The compose
//! `build` service mounts `<repo>/data` at `<spacetimedb>/data`, so the
//! relative path `../data/...` from `src/` resolves correctly inside the
//! build container. For host builds, create a symlink at
//! `spacetime/server/spacetimedb/data` → `../../../data` (or build only
//! inside the container).
//!
//! Adding a new `cards/NN.json` file requires appending an entry to
//! `CARDS_FILES` below.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde_json::Value;

use crate::packing::pack_definition;

// ---------- Aspects ----------

pub type AspectId = u8;

/// Sentinel id meaning "no aspect" / "unknown aspect". Aspect IDs are
/// 1-indexed.
pub const ASPECT_NONE: AspectId = 0;

#[derive(Debug, Clone)]
pub struct Aspect {
  pub id: AspectId,
  /// Programmatic name from the JSON, e.g. `"combat"`.
  pub name: String,
  /// Human-readable description from the JSON.
  pub description: String,
  /// Group the aspect was declared under, e.g. `"resources"`.
  pub group: String,
}

struct AspectRegistry {
  by_id: Vec<Aspect>,                      // index by (id - 1)
  id_by_name: BTreeMap<String, AspectId>,
}

const ASPECTS_JSON: &str = include_str!("../data/aspects.json");
static ASPECTS: OnceLock<Result<AspectRegistry, String>> = OnceLock::new();

fn aspects_registry() -> Result<&'static AspectRegistry, String> {
  ASPECTS.get_or_init(build_aspects).as_ref().map_err(|e| e.clone())
}

/// Look up an aspect's id by name. Returns `Ok(None)` if the registry built
/// successfully but no aspect with that name is declared, `Err` if the
/// aspect registry failed to build.
pub fn aspect_id(name: &str) -> Result<Option<AspectId>, String> {
  Ok(aspects_registry()?.id_by_name.get(name).copied())
}

/// Resolve an `AspectId` back to the full `Aspect` record. `Ok(None)` for
/// `ASPECT_NONE` and for ids past the end of the registry; `Err` on
/// registry-build failure.
pub fn aspect(id: AspectId) -> Result<Option<&'static Aspect>, String> {
  if id == ASPECT_NONE {
    return Ok(None);
  }
  Ok(aspects_registry()?.by_id.get((id - 1) as usize))
}

/// All known aspects, ordered by id. `Err` on registry-build failure.
pub fn aspects() -> Result<&'static [Aspect], String> {
  Ok(&aspects_registry()?.by_id)
}

fn build_aspects() -> Result<AspectRegistry, String> {
  let root: Value = serde_json::from_str(ASPECTS_JSON)
    .map_err(|e| format!("aspects.json: parse failed: {}", e))?;
  let root = root
    .as_object()
    .ok_or_else(|| "aspects.json: top-level not an object".to_string())?;

  let mut by_id: Vec<Aspect> = Vec::new();
  let mut id_by_name: BTreeMap<String, AspectId> = BTreeMap::new();
  let mut next_id: u32 = 1;

  for (group_name, group_value) in root {
    // Skip helper keys like "_comment" / "_rules".
    if group_name.starts_with('_') {
      continue;
    }
    let group_obj = group_value.as_object().ok_or_else(|| {
      format!("aspects.json: group {:?} not an object", group_name)
    })?;

    for (aspect_name, desc_value) in group_obj {
      if aspect_name.starts_with('_') {
        continue;
      }
      if next_id > AspectId::MAX as u32 {
        return Err(format!(
          "aspects.json: more than {} aspects (id overflow)",
          AspectId::MAX,
        ));
      }
      let id = next_id as AspectId;
      next_id += 1;

      let description = desc_value
        .as_str()
        .ok_or_else(|| {
          format!(
            "aspects.json: aspect {}/{} description not a string",
            group_name, aspect_name
          )
        })?
        .to_string();

      by_id.push(Aspect {
        id,
        name: aspect_name.clone(),
        description,
        group: group_name.clone(),
      });
      id_by_name.insert(aspect_name.clone(), id);
    }
  }

  Ok(AspectRegistry { by_id, id_by_name })
}

// ---------- Cards ----------

#[derive(Debug, Clone)]
pub struct CardDefinition {
  pub card_type: u8,
  pub card_category: u8,
  pub definition_id: u8,
  /// Programmatic key from the JSON, e.g. `"attack"`. Stable when the
  /// display `name` is renamed.
  pub key: String,
  /// Display name, e.g. `"Attack"`.
  pub name: String,
  /// Three CSS hex color codes for rendering. Validated as `#RRGGBB` at
  /// registry build time.
  pub style: [String; 3],
  /// `(aspect_id, value)` pairs. Names are translated to ids at registry
  /// build time via `aspect_id`; an unknown aspect name in card data is a
  /// stored registry-build error. Each `aspect_id` appears at most once
  /// per definition.
  pub aspects: Vec<(AspectId, i32)>,
}

const CARD_TYPES_JSON: &str = include_str!("../data/card_types.json");
const CARD_IDS_JSON: &str = include_str!("../data/cards/id.json");

/// Maximum valid id for a `card_type` or `card_category`. Both occupy the
/// `u4` halves of `packed_definition`, so 0xF is the hard cap.
const MAX_TYPE_OR_CATEGORY_ID: u64 = 0xF;

/// Every cards/*.json file compiled into the registry. Append a tuple here
/// when adding a new card data file. The filename is kept alongside the
/// content for clearer error messages on parse failure.
const CARDS_FILES: &[(&str, &str)] = &[
  ("cards/01.json", include_str!("../data/cards/01.json")),
];

struct CardRegistry {
  by_packed: BTreeMap<u16, CardDefinition>,
  /// `(type_id, category_id, key)` → `packed_definition`.
  by_path: BTreeMap<(u8, u8, String), u16>,
  /// Bare key → `packed_definition`, from `cards/id.json`.
  by_key: BTreeMap<String, u16>,
  type_ids: BTreeMap<String, u8>,
  category_ids: BTreeMap<String, u8>,
}

static CARDS: OnceLock<Result<CardRegistry, String>> = OnceLock::new();

fn cards_registry() -> Result<&'static CardRegistry, String> {
  CARDS.get_or_init(build_cards).as_ref().map_err(|e| e.clone())
}

/// Look up the `CardDefinition` for a `packed_definition`. `Ok(None)` for
/// the sentinel value 0, unknown `(card_type, card_category)`, or a
/// `definition_id` past the end of its bucket. `Err` on registry-build
/// failure.
pub fn decode_definition(packed: u16) -> Result<Option<&'static CardDefinition>, String> {
  Ok(cards_registry()?.by_packed.get(&packed))
}

/// Look up a card's `packed_definition` by its bare key (e.g. `"fatigue"`).
/// Uses the stable mapping from `cards/id.json` — O(log n), no scan needed.
/// Returns `Ok(None)` if the registry built but no card with that key exists;
/// `Err` on registry-build failure.
pub fn find_packed_by_key(card_key: &str) -> Result<Option<u16>, String> {
  Ok(cards_registry()?.by_key.get(card_key).copied())
}

/// Resolve a `"type/key"` or `"type/category/key"` string to the card's
/// `packed_definition`. Two-segment paths default the category to
/// `"default"`. Returns a descriptive `Err` for malformed paths,
/// unrecognized type / category / key, or registry-build failure.
pub fn find_packed(card_path: &str) -> Result<u16, String> {
  let parts: Vec<&str> = card_path.split('/').collect();
  let (type_name, category_name, card_key) = match parts.len() {
    2 => (parts[0], "default", parts[1]),
    3 => (parts[0], parts[1], parts[2]),
    _ => {
      return Err(format!(
        "invalid card path {:?}, expected 'type/key' or 'type/category/key'",
        card_path
      ));
    }
  };

  let registry = cards_registry()?;
  let &type_id = registry
    .type_ids
    .get(type_name)
    .ok_or_else(|| format!("unknown card type {:?}", type_name))?;
  let &category_id = registry
    .category_ids
    .get(category_name)
    .ok_or_else(|| format!("unknown card category {:?}", category_name))?;
  registry
    .by_path
    .get(&(type_id, category_id, card_key.to_string()))
    .copied()
    .ok_or_else(|| format!("unknown card {:?}", card_path))
}

fn build_cards() -> Result<CardRegistry, String> {
  let types_root: Value = serde_json::from_str(CARD_TYPES_JSON)
    .map_err(|e| format!("card_types.json: parse failed: {}", e))?;

  let type_ids = json_id_map(&types_root, "types")?;
  let category_ids = json_id_map(&types_root, "categories")?;

  // Load stable definition_id map — must exist (run gen-ids.py before building).
  // Format: { "<card_type>": { "<key>": <definition_id>, ... }, ... }
  let id_root: Value = serde_json::from_str(CARD_IDS_JSON)
    .map_err(|e| format!("cards/id.json: parse failed: {}", e))?;
  let id_obj = id_root
    .as_object()
    .ok_or_else(|| "cards/id.json: top-level not an object".to_string())?;
  let mut definition_ids: BTreeMap<String, BTreeMap<String, BTreeMap<String, u8>>> = BTreeMap::new();
  for (type_name, type_val) in id_obj {
    let type_obj = type_val
      .as_object()
      .ok_or_else(|| format!("cards/id.json: entry for type {:?} not an object", type_name))?;
    for (category_name, cat_val) in type_obj {
      let cat_obj = cat_val
        .as_object()
        .ok_or_else(|| format!("cards/id.json: entry for {:?}/{:?} not an object", type_name, category_name))?;
      let mut inner: BTreeMap<String, u8> = BTreeMap::new();
      for (key, val) in cat_obj {
        let n = val.as_u64().ok_or_else(|| {
          format!("cards/id.json: definition_id for {:?}/{:?}/{:?} not an integer", type_name, category_name, key)
        })?;
        if n == 0 || n > u8::MAX as u64 {
          return Err(format!(
            "cards/id.json: definition_id {} for {:?}/{:?}/{:?} out of range (1–255)",
            n, type_name, category_name, key
          ));
        }
        inner.insert(key.clone(), n as u8);
      }
      definition_ids
        .entry(type_name.clone())
        .or_default()
        .insert(category_name.clone(), inner);
    }
  }

  let mut by_packed: BTreeMap<u16, CardDefinition> = BTreeMap::new();
  let mut by_path: BTreeMap<(u8, u8, String), u16> = BTreeMap::new();
  let mut by_key: BTreeMap<String, u16> = BTreeMap::new();

  for (filename, content) in CARDS_FILES {
    let buckets: Value = serde_json::from_str(content)
      .map_err(|e| format!("{}: parse failed: {}", filename, e))?;
    let buckets = buckets
      .as_array()
      .ok_or_else(|| format!("{}: top-level not an array", filename))?;

    for bucket in buckets {
      let type_name = bucket["card_type"]
        .as_str()
        .ok_or_else(|| format!("{}: bucket missing 'card_type'", filename))?;
      let category_name = bucket["category"]
        .as_str()
        .ok_or_else(|| format!("{}: bucket missing 'category'", filename))?;

      // Buckets whose type or category isn't in card_types.json are silently
      // skipped — this lets card data files outpace the registry without
      // breaking the build, but means a typo'd bucket name simply won't
      // produce decodable cards. Fix card_types.json or the bucket name if
      // a definition isn't decoding.
      let Some(&card_type) = type_ids.get(type_name) else { continue };
      let Some(&card_category) = category_ids.get(category_name) else { continue };

      let cards_obj = bucket["cards"].as_object().ok_or_else(|| {
        format!(
          "{}: bucket {}/{}: 'cards' not an object",
          filename, type_name, category_name
        )
      })?;

      for (key, value) in cards_obj.iter() {
        let definition_id = definition_ids
          .get(type_name)
          .and_then(|m| m.get(category_name))
          .and_then(|m| m.get(key.as_str()))
          .copied()
          .ok_or_else(|| {
            format!(
              "{}: card {:?} ({:?}/{:?}) not found in cards/id.json — run gen-ids.py",
              filename, key, type_name, category_name
            )
          })?;
        let definition = parse_card(filename, value, card_type, card_category, definition_id, key)?;
        let packed = pack_definition(card_type, card_category, definition_id);
        by_packed.insert(packed, definition);
        by_path.insert((card_type, card_category, key.clone()), packed);
        by_key.insert(key.clone(), packed);
      }
    }
  }

  Ok(CardRegistry { by_packed, by_path, by_key, type_ids, category_ids })
}

/// Build a `name → id` map from a section of `card_types.json`.
///
/// Skips keys that begin with `_` (these are comments / placeholder
/// reservations like `_reserved_1`). Real entries — i.e. those whose key
/// doesn't start with `_` — must carry a numeric `id` field in `[0, 0xF]`;
/// missing or out-of-range ids are an error rather than a silent drop, so a
/// typo'd field name fails loudly.
fn json_id_map(root: &Value, section: &str) -> Result<BTreeMap<String, u8>, String> {
  let section_obj = root
    .get(section)
    .and_then(Value::as_object)
    .ok_or_else(|| format!("card_types.json: '{}' missing or not an object", section))?;

  let mut result = BTreeMap::new();
  for (name, info) in section_obj {
    if name.starts_with('_') {
      continue;
    }
    let id_value = info.get("id").ok_or_else(|| {
      format!("card_types.json: '{}' entry {:?} missing 'id'", section, name)
    })?;
    let id_u64 = id_value.as_u64().ok_or_else(|| {
      format!(
        "card_types.json: '{}' entry {:?} 'id' not a non-negative integer",
        section, name
      )
    })?;
    if id_u64 > MAX_TYPE_OR_CATEGORY_ID {
      return Err(format!(
        "card_types.json: '{}' entry {:?} id {} exceeds u4 max ({})",
        section, name, id_u64, MAX_TYPE_OR_CATEGORY_ID,
      ));
    }
    result.insert(name.clone(), id_u64 as u8);
  }
  Ok(result)
}

fn parse_card(
  filename: &str,
  value: &Value,
  card_type: u8,
  card_category: u8,
  definition_id: u8,
  key: &str,
) -> Result<CardDefinition, String> {
  let arr = value
    .as_array()
    .ok_or_else(|| format!("{}: card {}: spec not an array", filename, key))?;
  if arr.len() < 3 {
    return Err(format!(
      "{}: card {}: spec needs [name, style, aspects]",
      filename, key
    ));
  }

  let name = arr[0]
    .as_str()
    .ok_or_else(|| format!("{}: card {}: name not a string", filename, key))?
    .to_string();

  let style_arr = arr[1]
    .as_array()
    .ok_or_else(|| format!("{}: card {}: style not an array", filename, key))?;
  if style_arr.len() != 3 {
    return Err(format!(
      "{}: card {}: style needs exactly 3 entries",
      filename, key
    ));
  }
  let style: [String; 3] = [
    style_str(filename, key, style_arr, 0)?,
    style_str(filename, key, style_arr, 1)?,
    style_str(filename, key, style_arr, 2)?,
  ];

  let aspects_arr = arr[2]
    .as_array()
    .ok_or_else(|| format!("{}: card {}: aspects not an array", filename, key))?;

  let mut aspects: Vec<(AspectId, i32)> = Vec::with_capacity(aspects_arr.len());
  let mut seen_aspect_ids: BTreeSet<AspectId> = BTreeSet::new();
  for a in aspects_arr {
    let pair = a
      .as_array()
      .ok_or_else(|| format!("{}: card {}: aspect not an array", filename, key))?;
    if pair.len() != 2 {
      return Err(format!(
        "{}: card {}: aspect needs [name, value]",
        filename, key
      ));
    }
    let aspect_name = pair[0]
      .as_str()
      .ok_or_else(|| format!("{}: card {}: aspect name not a string", filename, key))?;
    let id = aspect_id(aspect_name)?.ok_or_else(|| {
      format!(
        "{}: card {}: unknown aspect {:?} (not declared in aspects.json)",
        filename, key, aspect_name
      )
    })?;
    if !seen_aspect_ids.insert(id) {
      return Err(format!(
        "{}: card {}: aspect {:?} listed more than once",
        filename, key, aspect_name
      ));
    }
    let aspect_value = pair[1].as_i64().ok_or_else(|| {
      format!("{}: card {}: aspect value not an integer", filename, key)
    })? as i32;
    aspects.push((id, aspect_value));
  }

  Ok(CardDefinition {
    card_type,
    card_category,
    definition_id,
    key: key.to_string(),
    name,
    style,
    aspects,
  })
}

fn style_str(filename: &str, key: &str, arr: &[Value], idx: usize) -> Result<String, String> {
  let s = arr[idx]
    .as_str()
    .ok_or_else(|| format!("{}: card {}: style[{}] not a string", filename, key, idx))?;
  if !is_valid_hex_color(s) {
    return Err(format!(
      "{}: card {}: style[{}] {:?} is not a valid #RRGGBB hex color",
      filename, key, idx, s
    ));
  }
  Ok(s.to_string())
}

/// `#RRGGBB` validator. Lowercase or uppercase hex, exactly 6 hex digits.
fn is_valid_hex_color(s: &str) -> bool {
  let bytes = s.as_bytes();
  if bytes.len() != 7 || bytes[0] != b'#' {
    return false;
  }
  bytes[1..].iter().all(|&b| b.is_ascii_hexdigit())
}

// ---------- Recipes ----------

/// A condition tree against a card. Used both to validate slot fillers
/// (where the tree is matched against a candidate card) and to drive
/// product generation (where `WeightedOr` selects between two outputs).
///
/// JSON grammar:
/// - `"corpus"` → `Card("corpus")` — match a card with this exact key
/// - `"any"` → `Any` — match any card (lowest specificity)
/// - `"@discipline"` → `Type(type_id)` — match any card whose `card_type`
///   resolves to the named type. Resolved at recipe-registry build time
///   against `card_types.json`; an unknown type is a build error.
/// - `["aspect", N]` → `Aspect(aspect_id("aspect"), N)` — match a card
///   whose aspect value is ≥ N
/// - `[E]` → just `E` (degenerate one-element array, common in slot
///   wrapping)
/// - `[E1, E2]` → `And(E1, E2)`
/// - `[E1, [], E2]` → `Or(E1, E2)`
/// - `[E1, [Wa, Wb], E2]` → `WeightedOr(E1, E2, Wa, Wb)` (intended for
///   products; treated as a non-weighted `Or` if used inside a slot match)
///
/// # Match specificity (used by the priority weighting in `actions.rs`)
///
/// When an entity matches a card, the per-leaf weight (more specific →
/// higher) is:
///
/// - `Card`: 4
/// - `Aspect`: 3
/// - `Type`: 2
/// - `Any`: 1
///
/// For composite entities, `And` sums the children's weights (slot is
/// more specific than either alone), `Or` / `WeightedOr` take the weight
/// of whichever branch satisfied (or 0 if neither did).
#[derive(Debug, Clone)]
pub enum Entity {
  Card(String),
  Aspect(AspectId, i32),
  /// Match any card whose `card_type` equals this `u8`. Resolved at
  /// recipe-build time so the matcher doesn't need a registry lookup
  /// per check.
  Type(u8),
  /// Match any card. Lowest specificity — used as a slot wildcard.
  Any,
  And(Box<Entity>, Box<Entity>),
  Or(Box<Entity>, Box<Entity>),
  WeightedOr {
    a: Box<Entity>,
    b: Box<Entity>,
    weight_a: u32,
    weight_b: u32,
  },
}

/// Determines what shape of trigger fires the recipe.
///
/// - `TopStack` / `BottomStack` — fired when the client submits a stack
///   via `submit_inventory_stacks`; the server tries to fit the slots
///   along the relevant branch.
/// - `OnCreate` — fired when a card is inserted via `insert_card_row`;
///   the new card itself is checked against the recipe's `root` entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeType {
  TopStack,
  BottomStack,
  OnCreate,
}

/// Where products from a completed action go. Resolved at completion time.
///
/// - `RootPanel` — owner of the chain root's inventory.
/// - `ActorPanel` — owner of the actor card's inventory.
///
/// More targets (e.g. `ActorWorld`, `RootWorld`) belong here when world
/// cards land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductTarget {
  RootPanel,
  ActorPanel,
}

#[derive(Debug, Clone)]
pub struct ProductGroup {
  pub target: ProductTarget,
  /// Each entity in this list produces one output card on completion.
  /// `WeightedOr` entities pick one alternative at random.
  pub entities: Vec<Entity>,
}

/// Recipe duration. Either fixed seconds or a list of `(seconds,
/// condition)` cases evaluated against an aspect pool, with a fallback at
/// the tail.
#[derive(Debug, Clone)]
pub enum Duration {
  Fixed(u32),
  Conditional {
    cases: Vec<(u32, Entity)>,
    fallback: u32,
  },
}

#[derive(Debug, Clone)]
pub struct RecipeDef {
  /// Stable ID from `recipes/id.json`. Stored in `Action.recipe` on the
  /// wire and never reassigned — safe across recipe additions and reorders.
  pub index: u32,
  /// Human-readable id from JSON, e.g. `"woodcutting"`.
  pub id: String,
  pub recipe_type: RecipeType,
  /// For `OnCreate`: the actor card must satisfy this entity. For stack
  /// types this is `None` unless the recipe wants to constrain the chain
  /// root separately from the slot list.
  pub root: Option<Entity>,
  /// Optional condition on the hex tile under the stack. Matched against
  /// the tile card's def (when world layer cards exist). Forward-looking:
  /// today no recipe data sets this and the matcher always scores
  /// `tile_weight = 0`. Top of the priority hierarchy: a satisfied
  /// `tile` outranks any combination of `root` and `slots` weights.
  pub tile: Option<Entity>,
  /// Slot list. Slot 1 is the actor; slots 2.. fill in chain order from
  /// the actor outward along the recipe's branch. Empty for `OnCreate`.
  pub slots: Vec<Entity>,
  /// 1-indexed slot positions consumed on completion. `0` means "the
  /// chain root", which is only meaningful when `root` is `Some`.
  pub reagents: Vec<u8>,
  pub products: Vec<ProductGroup>,
  pub duration: Duration,
}

const RECIPES_FILES: &[(&str, &str)] = &[
  ("recipes/01.json", include_str!("../data/recipes/01.json")),
];
const RECIPE_IDS_JSON: &str = include_str!("../data/recipes/id.json");

struct RecipeRegistry {
  /// Stable ID → recipe definition.
  by_id: BTreeMap<u32, RecipeDef>,
  /// Human-readable name → stable ID.
  id_by_name: BTreeMap<String, u32>,
  /// `RecipeType` (encoded as u8) → stable IDs in declaration order.
  by_type: BTreeMap<u8, Vec<u32>>,
}

static RECIPES: OnceLock<Result<RecipeRegistry, String>> = OnceLock::new();

fn recipes_registry() -> Result<&'static RecipeRegistry, String> {
  RECIPES.get_or_init(build_recipes).as_ref().map_err(|e| e.clone())
}

/// Look up a recipe by its stable ID (what `Action.recipe` stores).
/// Returns `Ok(None)` if no recipe with that ID is registered.
pub fn recipe(index: u32) -> Result<Option<&'static RecipeDef>, String> {
  Ok(recipes_registry()?.by_id.get(&index))
}

/// Look up a recipe by its human-readable id. `Ok(None)` if unknown.
pub fn find_recipe(id: &str) -> Result<Option<&'static RecipeDef>, String> {
  let registry = recipes_registry()?;
  let Some(&stable_id) = registry.id_by_name.get(id) else {
    return Ok(None);
  };
  Ok(registry.by_id.get(&stable_id))
}

/// All recipes of a given type, in declaration order.
pub fn recipes_of_type(rt: RecipeType) -> Result<Vec<&'static RecipeDef>, String> {
  let registry = recipes_registry()?;
  let key = recipe_type_key(rt);
  let Some(ids) = registry.by_type.get(&key) else {
    return Ok(Vec::new());
  };
  Ok(ids.iter().filter_map(|id| registry.by_id.get(id)).collect())
}

fn recipe_type_key(rt: RecipeType) -> u8 {
  match rt {
    RecipeType::TopStack => 0,
    RecipeType::BottomStack => 1,
    RecipeType::OnCreate => 2,
  }
}

fn build_recipes() -> Result<RecipeRegistry, String> {
  // Load stable ID map — must exist (run gen-ids.py before building).
  let ids_root: Value = serde_json::from_str(RECIPE_IDS_JSON)
    .map_err(|e| format!("recipes/id.json: parse failed: {}", e))?;
  let ids_obj = ids_root
    .as_object()
    .ok_or_else(|| "recipes/id.json: top-level not an object".to_string())?;
  let stable_ids: BTreeMap<String, u32> = ids_obj
    .iter()
    .map(|(name, val)| {
      let id = val
        .as_u64()
        .ok_or_else(|| format!("recipes/id.json: value for {:?} not an integer", name))?;
      Ok((name.clone(), id as u32))
    })
    .collect::<Result<_, String>>()?;

  // Pull `type_ids` from the cards registry — used by `parse_entity` to
  // resolve `"@<type_name>"` strings into `Entity::Type(<u8>)` at parse
  // time. This drives a transitive build of the card registry; if that
  // fails, recipe build fails too.
  let type_ids = cards_registry()?.type_ids.clone();

  let mut by_id: BTreeMap<u32, RecipeDef> = BTreeMap::new();
  let mut id_by_name: BTreeMap<String, u32> = BTreeMap::new();
  let mut by_type: BTreeMap<u8, Vec<u32>> = BTreeMap::new();

  for (filename, content) in RECIPES_FILES {
    let recipes_value: Value = serde_json::from_str(content)
      .map_err(|e| format!("{}: parse failed: {}", filename, e))?;
    let recipes_arr = recipes_value
      .as_array()
      .ok_or_else(|| format!("{}: top-level not an array", filename))?;

    for recipe_value in recipes_arr {
      let id = recipe_value["id"]
        .as_str()
        .ok_or_else(|| format!("{}: recipe missing 'id'", filename))?
        .to_string();

      let stable_id = stable_ids.get(&id).copied().ok_or_else(|| {
        format!(
          "{}: recipe {:?} not found in recipes/id.json — run gen-ids.py",
          filename, id
        )
      })?;

      if id_by_name.contains_key(&id) {
        return Err(format!(
          "{}: recipe id {:?} declared more than once",
          filename, id
        ));
      }

      let recipe_type = match recipe_value["type"].as_str() {
        Some("top_stack") => RecipeType::TopStack,
        Some("bottom_stack") => RecipeType::BottomStack,
        Some("on_create") => RecipeType::OnCreate,
        Some(other) => {
          return Err(format!(
            "{}: recipe {:?} unknown type {:?}",
            filename, id, other
          ));
        }
        None => {
          return Err(format!("{}: recipe {:?} missing 'type'", filename, id));
        }
      };

      let root = if recipe_value.get("root").is_some() {
        Some(parse_entity(&recipe_value["root"], &type_ids, filename, &id, "root")?)
      } else {
        None
      };

      let tile = if recipe_value.get("tile").is_some() {
        Some(parse_entity(&recipe_value["tile"], &type_ids, filename, &id, "tile")?)
      } else {
        None
      };

      let slots = if let Some(slots_arr) = recipe_value.get("slots").and_then(Value::as_array) {
        slots_arr
          .iter()
          .enumerate()
          .map(|(i, v)| parse_entity(v, &type_ids, filename, &id, &format!("slots[{}]", i)))
          .collect::<Result<Vec<_>, _>>()?
      } else {
        Vec::new()
      };

      let reagents = if let Some(arr) = recipe_value.get("reagents").and_then(Value::as_array) {
        arr
          .iter()
          .map(|v| {
            let n = v.as_u64().ok_or_else(|| {
              format!("{}: recipe {:?} reagents has non-integer entry: {:?}", filename, id, v)
            })?;
            if n > u8::MAX as u64 {
              return Err(format!(
                "{}: recipe {:?} reagent index {} exceeds u8 max",
                filename, id, n
              ));
            }
            Ok(n as u8)
          })
          .collect::<Result<Vec<_>, _>>()?
      } else {
        Vec::new()
      };

      let products = if let Some(products_obj) = recipe_value
        .get("products")
        .and_then(Value::as_object)
      {
        let mut groups: Vec<ProductGroup> = Vec::new();
        for (target_name, target_value) in products_obj {
          let target = match target_name.as_str() {
            "root_panel" => ProductTarget::RootPanel,
            "actor_panel" => ProductTarget::ActorPanel,
            other => {
              return Err(format!(
                "{}: recipe {:?} unknown product target {:?}",
                filename, id, other
              ));
            }
          };
          let entities_arr = target_value.as_array().ok_or_else(|| {
            format!(
              "{}: recipe {:?} products[{}] not an array",
              filename, id, target_name
            )
          })?;
          let entities = entities_arr
            .iter()
            .enumerate()
            .map(|(i, v)| {
              parse_entity(v, &type_ids, filename, &id, &format!("products[{}][{}]", target_name, i))
            })
            .collect::<Result<Vec<_>, _>>()?;
          groups.push(ProductGroup { target, entities });
        }
        groups
      } else {
        Vec::new()
      };

      let duration = parse_duration(&recipe_value["duration"], &type_ids, filename, &id)?;

      let def = RecipeDef {
        index: stable_id,
        id: id.clone(),
        recipe_type,
        root,
        tile,
        slots,
        reagents,
        products,
        duration,
      };
      by_type
        .entry(recipe_type_key(recipe_type))
        .or_default()
        .push(stable_id);
      id_by_name.insert(id, stable_id);
      by_id.insert(stable_id, def);
    }
  }

  Ok(RecipeRegistry { by_id, id_by_name, by_type })
}

/// Sentinel string parsed as `Entity::Any`. Reserved — a card with this
/// key would shadow the wildcard.
const ENTITY_ANY_LITERAL: &str = "any";
/// Prefix marking a string as `Entity::Type(<typename>)`. The remainder
/// of the string after `@` is looked up in the card-type registry at
/// recipe-build time.
const ENTITY_TYPE_PREFIX: char = '@';

fn parse_entity(
  value: &Value,
  type_ids: &BTreeMap<String, u8>,
  filename: &str,
  recipe_id: &str,
  path: &str,
) -> Result<Entity, String> {
  if let Some(s) = value.as_str() {
    if s == ENTITY_ANY_LITERAL {
      return Ok(Entity::Any);
    }
    if let Some(type_name) = s.strip_prefix(ENTITY_TYPE_PREFIX) {
      let &type_id = type_ids.get(type_name).ok_or_else(|| {
        format!(
          "{}: recipe {:?} {}: unknown card type {:?} (not declared in card_types.json)",
          filename, recipe_id, path, type_name
        )
      })?;
      return Ok(Entity::Type(type_id));
    }
    return Ok(Entity::Card(s.to_string()));
  }
  let arr = value.as_array().ok_or_else(|| {
    format!(
      "{}: recipe {:?} {}: entity not a string or array: {:?}",
      filename, recipe_id, path, value
    )
  })?;
  match arr.len() {
    1 => parse_entity(&arr[0], type_ids, filename, recipe_id, path),
    2 => {
      // Disambiguate `[string, number]` (aspect check) from
      // `[entity, entity]` (AND). Numbers aren't valid entities, so a
      // numeric second element pins it to the aspect form.
      if let (Some(s), Some(n)) = (arr[0].as_str(), arr[1].as_i64()) {
        let id = aspect_id(s)?.ok_or_else(|| {
          format!(
            "{}: recipe {:?} {}: unknown aspect {:?} (not declared in aspects.json)",
            filename, recipe_id, path, s
          )
        })?;
        Ok(Entity::Aspect(id, n as i32))
      } else {
        let a = parse_entity(&arr[0], type_ids, filename, recipe_id, &format!("{}[0]", path))?;
        let b = parse_entity(&arr[1], type_ids, filename, recipe_id, &format!("{}[1]", path))?;
        Ok(Entity::And(Box::new(a), Box::new(b)))
      }
    }
    3 => {
      let middle = &arr[1];
      let a = parse_entity(&arr[0], type_ids, filename, recipe_id, &format!("{}[0]", path))?;
      let b = parse_entity(&arr[2], type_ids, filename, recipe_id, &format!("{}[2]", path))?;
      let middle_arr = middle.as_array().ok_or_else(|| {
        format!(
          "{}: recipe {:?} {}: 3-tuple middle not an array: {:?}",
          filename, recipe_id, path, middle
        )
      })?;
      if middle_arr.is_empty() {
        Ok(Entity::Or(Box::new(a), Box::new(b)))
      } else if middle_arr.len() == 2 {
        let weight_a = middle_arr[0].as_u64().ok_or_else(|| {
          format!(
            "{}: recipe {:?} {}: weight[0] not a non-negative integer: {:?}",
            filename, recipe_id, path, middle_arr[0]
          )
        })? as u32;
        let weight_b = middle_arr[1].as_u64().ok_or_else(|| {
          format!(
            "{}: recipe {:?} {}: weight[1] not a non-negative integer: {:?}",
            filename, recipe_id, path, middle_arr[1]
          )
        })? as u32;
        Ok(Entity::WeightedOr {
          a: Box::new(a),
          b: Box::new(b),
          weight_a,
          weight_b,
        })
      } else {
        Err(format!(
          "{}: recipe {:?} {}: 3-tuple middle has {} elements, expected 0 (Or) or 2 (WeightedOr)",
          filename,
          recipe_id,
          path,
          middle_arr.len()
        ))
      }
    }
    _ => Err(format!(
      "{}: recipe {:?} {}: entity array of length {} not supported",
      filename,
      recipe_id,
      path,
      arr.len()
    )),
  }
}

fn parse_duration(
  value: &Value,
  type_ids: &BTreeMap<String, u8>,
  filename: &str,
  recipe_id: &str,
) -> Result<Duration, String> {
  // Fixed: bare number.
  if let Some(n) = value.as_u64() {
    return Ok(Duration::Fixed(n as u32));
  }

  // Conditional: array of `[seconds, condition]` cases plus a trailing
  // bare-number fallback.
  let arr = value.as_array().ok_or_else(|| {
    format!(
      "{}: recipe {:?} duration not a number or array: {:?}",
      filename, recipe_id, value
    )
  })?;

  if arr.is_empty() {
    return Err(format!(
      "{}: recipe {:?} duration is an empty array",
      filename, recipe_id
    ));
  }

  let mut cases: Vec<(u32, Entity)> = Vec::new();
  let mut fallback: Option<u32> = None;

  for (i, entry) in arr.iter().enumerate() {
    if let Some(n) = entry.as_u64() {
      if i != arr.len() - 1 {
        return Err(format!(
          "{}: recipe {:?} duration[{}] is a bare number; only the trailing entry can be the fallback",
          filename, recipe_id, i
        ));
      }
      fallback = Some(n as u32);
      continue;
    }

    let case = entry.as_array().ok_or_else(|| {
      format!(
        "{}: recipe {:?} duration[{}] not a number or [seconds, condition]: {:?}",
        filename, recipe_id, i, entry
      )
    })?;
    if case.len() != 2 {
      return Err(format!(
        "{}: recipe {:?} duration[{}] not a 2-element [seconds, condition]",
        filename, recipe_id, i
      ));
    }
    let secs = case[0].as_u64().ok_or_else(|| {
      format!(
        "{}: recipe {:?} duration[{}][0] not a non-negative integer: {:?}",
        filename, recipe_id, i, case[0]
      )
    })? as u32;
    let cond = parse_entity(&case[1], type_ids, filename, recipe_id, &format!("duration[{}][1]", i))?;
    cases.push((secs, cond));
  }

  let fallback = fallback.ok_or_else(|| {
    format!(
      "{}: recipe {:?} duration: no trailing fallback (last entry must be a bare number)",
      filename, recipe_id
    )
  })?;

  Ok(Duration::Conditional { cases, fallback })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn hex_color_validator() {
    assert!(is_valid_hex_color("#000000"));
    assert!(is_valid_hex_color("#FFFFFF"));
    assert!(is_valid_hex_color("#ffffff"));
    assert!(is_valid_hex_color("#a8E0e6"));
    assert!(!is_valid_hex_color("000000"));
    assert!(!is_valid_hex_color("#00000"));
    assert!(!is_valid_hex_color("#0000000"));
    assert!(!is_valid_hex_color("#GGGGGG"));
    assert!(!is_valid_hex_color(""));
    assert!(!is_valid_hex_color("#"));
  }
}
