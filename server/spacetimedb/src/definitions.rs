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

use crate::packing::{pack_definition, pack_recipe, RECIPE_ID_MASK, RECIPE_TYPE_OR_CATEGORY_MASK};

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
  /// `type_id` → shape (`"rect"` or `"hex"`) from `card_types.json`.
  /// Drives [`is_hex_type`]; missing types default to `"rect"`.
  type_shapes: BTreeMap<u8, String>,
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

/// Whether the given `card_type` id resolves to a hex-shaped type
/// (`"hex"` in `card_types.json`). Used by `magnetic.rs` to decide
/// whether the action's actor is a hex anchor and slot[0] should be
/// attached as a hex-root rather than stacked top/bottom. Unknown
/// type ids default to `false` (rect-like) so a stale `packed_definition`
/// can't accidentally trip hex-specific paths.
pub fn is_hex_type(type_id: u8) -> Result<bool, String> {
  Ok(cards_registry()?
    .type_shapes
    .get(&type_id)
    .map_or(false, |s| s == "hex"))
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
  let type_shapes = json_type_shapes(&types_root)?;

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

  Ok(CardRegistry { by_packed, by_path, by_key, type_ids, category_ids, type_shapes })
}

/// Build a `type_id → shape` map from `card_types.json`'s `types`
/// section. Skips reserved/comment keys and entries without a `shape`
/// field. Mirrors the structure of [`json_id_map`] but pulls a
/// different field.
fn json_type_shapes(root: &Value) -> Result<BTreeMap<u8, String>, String> {
  let types_obj = root
    .get("types")
    .and_then(Value::as_object)
    .ok_or_else(|| "card_types.json: 'types' missing or not an object".to_string())?;
  let mut result = BTreeMap::new();
  for (name, info) in types_obj {
    if name.starts_with('_') {
      continue;
    }
    let id = info
      .get("id")
      .and_then(Value::as_u64)
      .ok_or_else(|| format!("card_types.json: types.{:?} missing 'id'", name))?;
    if id > MAX_TYPE_OR_CATEGORY_ID {
      continue;
    }
    if let Some(shape) = info.get("shape").and_then(Value::as_str) {
      result.insert(id as u8, shape.to_string());
    }
  }
  Ok(result)
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
/// - `Stack(Up)` / `Stack(Down)` — fired when the client submits a
///   stack via `submit_inventory_stacks`; the server tries to fit the
///   slots along the up- or down-branch from the submitted root.
/// - `OnCreate` — fired when a card is inserted via `insert_card_row`;
///   the new card itself is checked against the recipe's `hex`
///   and/or `root` entity. At least one of those two must be set
///   (parser-enforced): `hex` requires the new card to be a hex-shaped
///   type matching the entity, `root` matches any type. An `OnCreate`
///   recipe with a non-`None` `magnetic` field doubles as a magnetic
///   recipe — the matched action installs the slot-fill ticker (see
///   `magnetic.rs`) on the new card, and the inner recipes describe
///   what the server pulls from the player's inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeType {
  Stack(StackDirection),
  OnCreate,
}

/// Which way a stack recipe walks the chain from the submitted root.
/// `Up` matches what the JSON schema calls the `up` direction (the
/// player has stacked cards "above" the root); `Down` matches `down`.
/// The pair of values mirrors `InventoryStack { stack_up, stack_down }`
/// on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackDirection {
  Up,
  Down,
}

/// Magnetic recipes nest a bucket-style sub-tree of *inner* recipes
/// inside the outer recipe's `magnetic` field. The outer is a normal
/// recipe (matched against the chain like anything else); the inner
/// recipes describe what cards the magnetic phase pulls from the
/// player's inventory and stacks onto the magnetic action's anchor.
///
/// Schema:
///
/// ```json
/// "magnetic": {
///   "type": "stack",
///   "up":   [ {inner}, {inner}, … ],
///   "down": [ {inner}, … ]
/// }
/// ```
///
/// At parse time we flatten the directional arrays into a single
/// `inners` list, baking each inner's direction into its
/// `recipe_type`. The flat order is the same order the parser walked,
/// which is the index used for the *sub-id* stored in the queued
/// inner action's `flags` (high 4 bits — capped at 16 inners per
/// outer).
#[derive(Debug, Clone)]
pub struct MagneticBucket {
  pub inners: Vec<InnerRecipe>,
}

/// One inner recipe inside a [`MagneticBucket`]. Like a top-level
/// recipe but without `id` (sub-identified by its position in
/// `MagneticBucket.inners`), without nested `magnetic` (the design
/// doesn't recurse — magnetic recipes can't themselves contain
/// magnetic recipes), and without `interval` (only the outer carries
/// the magnetic phase cadence).
///
/// `recipe_type` is baked in from the bucket's direction key at parse
/// time: under `"up"` it's `Stack(Up)`, under `"down"` it's
/// `Stack(Down)`, under `"self"` it's `OnCreate`. The tick uses this
/// to know which way to walk the chain when matching slot fillers.
///
/// `duration` is the inner action's duration (in seconds) once the
/// magnetic phase queues it into `actions` — distinct from the outer
/// recipe's `duration`, which is the magnetic-phase loop-count cap.
#[derive(Debug, Clone)]
pub struct InnerRecipe {
  pub recipe_type: RecipeType,
  pub root: Option<Entity>,
  pub hex: Option<Entity>,
  pub slots: Vec<Entity>,
  pub reagents: Vec<Reagent>,
  pub products: Vec<ProductGroup>,
  pub duration: Duration,
}

/// Where products from a completed action go. Two independent axes:
///
/// - `place` — what *kind* of destination (an inventory panel today;
///   future: a hex tile, a loose world spot, a player's pile, …).
/// - `owner` — which referent of the action defines that destination
///   (the chain root, the actor, future: the underlying tile, …).
///
/// The recipe JSON encodes this as a nested map:
///
/// ```json
/// "products": {
///   "inventory": {
///     "root":  [/* entities */],
///     "actor": [/* entities */]
///   }
/// }
/// ```
///
/// Outer key picks the [`ProductPlace`], inner key picks the
/// [`ProductOwner`]. Adding a new place (e.g. `"hex"`) or a new owner
/// (e.g. `"hex"` to mean "the tile under the action") is one enum
/// variant + one match arm in `actions::resolve_product_destination`,
/// not a new flat target name per combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductTarget {
  pub place: ProductPlace,
  pub owner: ProductOwner,
}

/// What *kind* of destination a product lands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductPlace {
  /// A player's inventory panel. Combined with [`ProductOwner`] to
  /// pick *which* player's panel. Today this is the only supported
  /// place; world-tile and loose-world placements will land alongside
  /// the world board.
  Inventory,
}

/// Which action-relative referent the product is attached to. Each
/// variant resolves to a player_id (the panel owner) at completion
/// time; the destination is always the inventory at `LAYER_INVENTORY`.
///
/// JSON keys: `"root"`, `"actor"`, `"hex"`, `"action"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductOwner {
  /// Chain root's owner. For `OnCreate` (root == actor), resolves to
  /// the actor's `owner_id`. For stack recipes the chain root isn't
  /// held by the action and isn't recoverable from server state at
  /// completion, so this currently falls back to the action owner —
  /// distinct from `Actor` only when a future change persists the
  /// root id on `ActionScheduler`.
  Root,
  /// Actor card's owner — `Card.owner_id` of `action.card_id`.
  Actor,
  /// Hex card's owner. The matcher persists the resolved hex id on
  /// `ActionScheduler.hex_card_id` at start time; completion looks
  /// it up and reads `hex_card.owner_id`. Falls back to the action
  /// owner when the chain isn't on a hex, the hex is unowned
  /// (`owner_id == 0`), or the hex resolved from a `Zone` cell
  /// (which doesn't carry an `owner_id`).
  Hex,
  /// Action's owner — `Action.owner_id`, set by `start_action`.
  /// Always present; the most reliable fallback when the
  /// card-relative owners can't be resolved.
  Action,
}

#[derive(Debug, Clone)]
pub struct ProductGroup {
  pub target: ProductTarget,
  /// Each entity in this list produces one output card on completion.
  /// `WeightedOr` entities pick one alternative at random.
  pub entities: Vec<Entity>,
}

/// What a recipe consumes on completion. Three kinds, all optional:
///
/// - `Root` — the chain root card. For stack recipes that's the
///   submitted root (which isn't held by a `CardHold` today, so this
///   is a no-op for stack types until chain context-at-completion
///   lands). For `OnCreate`, root and actor are the same card, so
///   this resolves to `action.card_id`.
/// - `Hex` — the hex card the action is anchored to (recorded on
///   `ActionScheduler.hex_card_id` at start time).
/// - `Slot(N)` — the 1-indexed slot position. Slot 1 is always the
///   actor (`action.card_id`); slot 2+ requires per-slot claim
///   tracking that doesn't exist yet.
///
/// JSON form: strings `"root"` and `"hex"` for the named referents,
/// integers `1..=255` for slot positions. Integer `0` is no longer
/// accepted — use `"root"` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reagent {
  Root,
  Hex,
  Slot(u8),
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
  /// Packed stable ID. Layout (see [`crate::packing::pack_recipe`]):
  /// `[recipe_type:u3][recipe_category:u3][recipe_id:u10]`. The
  /// `recipe_id` (low 10 bits) comes from `recipes/id.json`; the
  /// `recipe_type` and `recipe_category` (high 6 bits) come from
  /// `recipe_types.json` and identify the bucket the recipe was
  /// declared under. Stored in `Action.recipe` and
  /// `MagneticAction.recipe` on the wire; never reassigned — safe
  /// across recipe additions and reorders.
  pub index: u16,
  /// Human-readable id from JSON, e.g. `"woodcutting"`.
  pub id: String,
  pub recipe_type: RecipeType,
  /// For `OnCreate`: when set, the new card itself must satisfy this
  /// entity (no shape constraint). At least one of `root` / `hex`
  /// must be set for `OnCreate`. For stack types this is `None`
  /// unless the recipe wants to constrain the chain root separately
  /// from the slot list.
  pub root: Option<Entity>,
  /// Optional hex tier. Semantics depend on `recipe_type`:
  ///
  /// - **Stack**: a condition on the hex card the chain root is
  ///   attached to. A rectangle root with `stacked_state == 3` carries
  ///   the hex card's id in `micro_location`; the matcher resolves
  ///   that and scores this entity against the hex card's definition.
  /// - **OnCreate**: a condition on the *new card itself* — it must
  ///   be a hex-shaped type ([`is_hex_type`] returns `true`) and its
  ///   def must satisfy the entity. Matching here installs the
  ///   action / magnetic_action with the new card as the anchor.
  ///
  /// When `None`, the hex tier contributes 0. When `Some(_)`, it's
  /// the top of the priority hierarchy — a satisfied `hex` outranks
  /// any combination of `root` and `slots` weights.
  pub hex: Option<Entity>,
  /// Slot list. For `Stack(_)` recipes, slot 1 is the actor; slots 2..
  /// fill in chain order from the actor outward along the recipe's
  /// branch direction. Empty for non-magnetic `OnCreate`. **For
  /// magnetic recipes** (`magnetic.is_some()`), the slots describe the
  /// inputs the server pulls from the player's inventory — the actor
  /// is *not* in this list — and `slots[0]` is the first magnetic
  /// input, stacked on the actor (or attached as a hex root if the
  /// actor is hex-shaped) and so on.
  pub slots: Vec<Entity>,
  /// What the recipe consumes on completion. See [`Reagent`] —
  /// strings `"root"` / `"hex"` and 1-indexed slot integers in JSON.
  pub reagents: Vec<Reagent>,
  pub products: Vec<ProductGroup>,
  /// Action duration in seconds. Optional **only** for outer magnetic
  /// recipes (where `magnetic.is_some()`), and there it acts as the
  /// magnetic-phase loop-count cap (in ticks, not seconds). For
  /// non-magnetic recipes this is the seconds-from-start the action
  /// runs in `actions` before completion fires; the parser requires
  /// it for those. `None` on a magnetic outer means "no terminator —
  /// magnetic action runs until it queues an inner or is cancelled".
  pub duration: Option<Duration>,
  /// When set, this recipe is *magnetic*: `magnetic.rs` installs a
  /// scheduled tick that pulls inventory cards into the action's
  /// chain per the bucket's inner recipes, then queues an inner
  /// action into `actions` once any inner's slot list is fully
  /// filled. The outer recipe's `slots` / `reagents` / `products`
  /// describe the magnetic *outer*: matched at install time, fired
  /// at magnetic-action completion (queue or timeout). The inner
  /// recipe's fields fire at the queued inner action's completion.
  pub magnetic: Option<MagneticBucket>,
  /// Tick cadence in seconds for the magnetic phase. Only meaningful
  /// when `magnetic.is_some()`; ignored otherwise. The magnetic_action
  /// schedules a tick every `interval` seconds; each tick attempts
  /// one card pickup. Required for magnetic recipes.
  pub interval: Option<u32>,
}

const RECIPES_FILES: &[(&str, &str)] = &[
  ("recipes/01.json", include_str!("../data/recipes/01.json")),
];
const RECIPE_IDS_JSON: &str = include_str!("../data/recipes/id.json");
const RECIPE_TYPES_JSON: &str = include_str!("../data/recipe_types.json");

struct RecipeRegistry {
  /// Packed stable ID → recipe definition. Key is the same `u16`
  /// `Action.recipe` carries on the wire (see
  /// [`crate::packing::pack_recipe`]).
  by_id: BTreeMap<u16, RecipeDef>,
  /// Human-readable name → packed stable ID.
  id_by_name: BTreeMap<String, u16>,
  /// `(type_id, category_id)` → packed stable IDs in declaration order.
  by_type: BTreeMap<(u8, u8), Vec<u16>>,
}

static RECIPES: OnceLock<Result<RecipeRegistry, String>> = OnceLock::new();

fn recipes_registry() -> Result<&'static RecipeRegistry, String> {
  RECIPES.get_or_init(build_recipes).as_ref().map_err(|e| e.clone())
}

/// Look up a recipe by its packed stable ID (what `Action.recipe`
/// stores). Returns `Ok(None)` if no recipe with that ID is registered.
pub fn recipe(index: u16) -> Result<Option<&'static RecipeDef>, String> {
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
  let key = recipe_type_pair(rt)?;
  let Some(ids) = registry.by_type.get(&key) else {
    return Ok(Vec::new());
  };
  Ok(ids.iter().filter_map(|id| registry.by_id.get(id)).collect())
}

/// Resolve a `RecipeType` variant to its `(type_id, category_id)` pair
/// from `recipe_types.json`. The pair is what `pack_recipe` puts in
/// the high 6 bits of a packed recipe id.
fn recipe_type_pair(rt: RecipeType) -> Result<(u8, u8), String> {
  let registry = recipe_types_registry()?;
  let (type_name, category_name) = recipe_type_names(rt);
  let &type_id = registry.types.get(type_name).ok_or_else(|| {
    format!("recipe_types.json: type {:?} missing — required by RecipeType variant", type_name)
  })?;
  let &category_id = registry.categories.get(category_name).ok_or_else(|| {
    format!(
      "recipe_types.json: category {:?} missing — required by RecipeType variant",
      category_name
    )
  })?;
  Ok((type_id, category_id))
}

/// JSON-side names of a `RecipeType` variant — the bucket type and
/// direction key it was declared under.
fn recipe_type_names(rt: RecipeType) -> (&'static str, &'static str) {
  match rt {
    RecipeType::Stack(StackDirection::Up) => ("stack", "up"),
    RecipeType::Stack(StackDirection::Down) => ("stack", "down"),
    RecipeType::OnCreate => ("on_create", "self"),
  }
}

// ---------- Recipe types registry ----------

struct RecipeTypeRegistry {
  /// `name → recipe_type_id` (3 bits, from `recipe_types.json`'s
  /// `types` section).
  types: BTreeMap<String, u8>,
  /// `name → recipe_category_id` (3 bits, from `recipe_types.json`'s
  /// `categories` section).
  categories: BTreeMap<String, u8>,
}

static RECIPE_TYPES: OnceLock<Result<RecipeTypeRegistry, String>> = OnceLock::new();

fn recipe_types_registry() -> Result<&'static RecipeTypeRegistry, String> {
  RECIPE_TYPES
    .get_or_init(build_recipe_types)
    .as_ref()
    .map_err(|e| e.clone())
}

fn build_recipe_types() -> Result<RecipeTypeRegistry, String> {
  let root: Value = serde_json::from_str(RECIPE_TYPES_JSON)
    .map_err(|e| format!("recipe_types.json: parse failed: {}", e))?;
  let types = recipe_id_section(&root, "types")?;
  let categories = recipe_id_section(&root, "categories")?;
  Ok(RecipeTypeRegistry { types, categories })
}

/// Read a `name → id` map from one section of `recipe_types.json`.
/// Skips reserved/comment keys (those starting with `_`); requires
/// real entries to carry an integer `id` field that fits in 3 bits.
fn recipe_id_section(root: &Value, section: &str) -> Result<BTreeMap<String, u8>, String> {
  let section_obj = root
    .get(section)
    .and_then(Value::as_object)
    .ok_or_else(|| format!("recipe_types.json: '{}' missing or not an object", section))?;
  let mut result = BTreeMap::new();
  for (name, info) in section_obj {
    if name.starts_with('_') {
      continue;
    }
    let id_value = info.get("id").ok_or_else(|| {
      format!("recipe_types.json: '{}' entry {:?} missing 'id'", section, name)
    })?;
    let id_u64 = id_value.as_u64().ok_or_else(|| {
      format!(
        "recipe_types.json: '{}' entry {:?} 'id' not a non-negative integer",
        section, name
      )
    })?;
    if id_u64 > RECIPE_TYPE_OR_CATEGORY_MASK as u64 {
      return Err(format!(
        "recipe_types.json: '{}' entry {:?} id {} exceeds u3 max ({})",
        section, name, id_u64, RECIPE_TYPE_OR_CATEGORY_MASK,
      ));
    }
    result.insert(name.clone(), id_u64 as u8);
  }
  Ok(result)
}

fn build_recipes() -> Result<RecipeRegistry, String> {
  // Build the type+category id map first — we need it to pack each
  // recipe's stable ID and to validate that a bucket's type/direction
  // is actually declared in `recipe_types.json`.
  let type_registry = recipe_types_registry()?;

  // Walk `recipes/id.json` (`{ "<type>": { "<category>": { "<key>":
  // <id>, … }, … }, … }`) and flatten it into a single `name →
  // packed_u16` map. The packed value is what `Action.recipe` carries
  // on the wire and what we'll store in `RecipeDef.index`.
  let ids_root: Value = serde_json::from_str(RECIPE_IDS_JSON)
    .map_err(|e| format!("recipes/id.json: parse failed: {}", e))?;
  let ids_obj = ids_root
    .as_object()
    .ok_or_else(|| "recipes/id.json: top-level not an object".to_string())?;

  let mut packed_ids: BTreeMap<String, u16> = BTreeMap::new();
  for (type_name, type_val) in ids_obj {
    let &type_id = type_registry.types.get(type_name).ok_or_else(|| {
      format!(
        "recipes/id.json: type {:?} not declared in recipe_types.json",
        type_name
      )
    })?;
    let type_obj = type_val.as_object().ok_or_else(|| {
      format!("recipes/id.json: entry for type {:?} not an object", type_name)
    })?;
    for (category_name, cat_val) in type_obj {
      let &category_id = type_registry.categories.get(category_name).ok_or_else(|| {
        format!(
          "recipes/id.json: category {:?} (under type {:?}) not declared in recipe_types.json",
          category_name, type_name
        )
      })?;
      let cat_obj = cat_val.as_object().ok_or_else(|| {
        format!(
          "recipes/id.json: entry for {:?}/{:?} not an object",
          type_name, category_name
        )
      })?;
      for (key, val) in cat_obj {
        let n = val.as_u64().ok_or_else(|| {
          format!(
            "recipes/id.json: id for {:?}/{:?}/{:?} not an integer",
            type_name, category_name, key
          )
        })?;
        if n == 0 || n > RECIPE_ID_MASK as u64 {
          return Err(format!(
            "recipes/id.json: id {} for {:?}/{:?}/{:?} out of range (1..={})",
            n, type_name, category_name, key, RECIPE_ID_MASK,
          ));
        }
        let packed = pack_recipe(type_id, category_id, n as u16);
        if let Some(prev) = packed_ids.insert(key.clone(), packed) {
          return Err(format!(
            "recipes/id.json: recipe key {:?} declared more than once (prev packed={:#06x}, new={:#06x})",
            key, prev, packed,
          ));
        }
      }
    }
  }

  // Pull `type_ids` from the cards registry — used by `parse_entity` to
  // resolve `"@<type_name>"` strings into `Entity::Type(<u8>)` at parse
  // time. This drives a transitive build of the card registry; if that
  // fails, recipe build fails too.
  let type_ids = cards_registry()?.type_ids.clone();

  let mut by_id: BTreeMap<u16, RecipeDef> = BTreeMap::new();
  let mut id_by_name: BTreeMap<String, u16> = BTreeMap::new();
  let mut by_type: BTreeMap<(u8, u8), Vec<u16>> = BTreeMap::new();

  for (filename, content) in RECIPES_FILES {
    let buckets_value: Value = serde_json::from_str(content)
      .map_err(|e| format!("{}: parse failed: {}", filename, e))?;
    let buckets = buckets_value
      .as_array()
      .ok_or_else(|| format!("{}: top-level not an array of buckets", filename))?;

    for bucket in buckets {
      let bucket_type = bucket
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{}: bucket missing 'type'", filename))?;

      // Each bucket maps `type` to one or more direction-keyed arrays
      // of recipes. The pairs below say "for each direction key the
      // bucket's type allows, find the recipe array under that key
      // and tag its entries with this RecipeType."
      let direction_keys: &[(&str, RecipeType)] = match bucket_type {
        "stack" => &[
          ("up", RecipeType::Stack(StackDirection::Up)),
          ("down", RecipeType::Stack(StackDirection::Down)),
        ],
        "on_create" => &[("self", RecipeType::OnCreate)],
        other => {
          return Err(format!(
            "{}: bucket has unknown type {:?}, expected \"stack\" or \"on_create\"",
            filename, other,
          ));
        }
      };

      for &(direction_key, recipe_type) in direction_keys {
        let Some(arr) = bucket.get(direction_key).and_then(Value::as_array) else {
          continue;
        };
        let pair = recipe_type_pair(recipe_type)?;
        for recipe_value in arr {
          let (id, stable_id, def) = parse_recipe(
            recipe_value,
            recipe_type,
            filename,
            &type_ids,
            &packed_ids,
          )?;
          if id_by_name.contains_key(&id) {
            return Err(format!(
              "{}: recipe id {:?} declared more than once",
              filename, id
            ));
          }
          by_type.entry(pair).or_default().push(stable_id);
          id_by_name.insert(id, stable_id);
          by_id.insert(stable_id, def);
        }
      }
    }
  }

  Ok(RecipeRegistry { by_id, id_by_name, by_type })
}

/// Parse one recipe record from inside a direction-keyed bucket array
/// (`up` / `down` for `stack`, `self` for `on_create`). The
/// surrounding bucket has already supplied the `recipe_type`; the
/// record itself no longer carries a `type` field. Returns
/// `(id, stable_id, def)` for the caller to register. `stable_ids` is
/// the packed-u16 map built by `build_recipes` from the nested
/// `recipes/id.json` (see [`pack_recipe`] for the layout).
fn parse_recipe(
  recipe_value: &Value,
  recipe_type: RecipeType,
  filename: &str,
  type_ids: &BTreeMap<String, u8>,
  stable_ids: &BTreeMap<String, u16>,
) -> Result<(String, u16, RecipeDef), String> {
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

  // `magnetic` is a nested bucket-style sub-tree. Same shape as the
  // top-level recipe file (a `type` plus direction-keyed inner-recipe
  // arrays), parsed into a flat `MagneticBucket.inners` list with each
  // inner's `recipe_type` baked from the bucket's direction key.
  let magnetic = if let Some(mag_value) = recipe_value.get("magnetic") {
    let mag_obj = mag_value.as_object().ok_or_else(|| {
      format!(
        "{}: recipe {:?} 'magnetic' not an object",
        filename, id
      )
    })?;
    Some(parse_magnetic_bucket(mag_obj, filename, &id, type_ids)?)
  } else {
    None
  };

  // `interval` (seconds) — required when `magnetic.is_some()`, ignored
  // otherwise. Drives the magnetic_action's recurring schedule.
  let interval = match recipe_value.get("interval") {
    Some(v) => {
      let n = v.as_u64().ok_or_else(|| {
        format!(
          "{}: recipe {:?} 'interval' not a non-negative integer: {:?}",
          filename, id, v
        )
      })?;
      Some(u32::try_from(n).map_err(|_| {
        format!(
          "{}: recipe {:?} 'interval' value {} exceeds u32 range",
          filename, id, n
        )
      })?)
    }
    None => None,
  };
  if magnetic.is_some() && interval.is_none() {
    return Err(format!(
      "{}: magnetic recipe {:?} missing required 'interval' field",
      filename, id
    ));
  }
  if magnetic.is_none() && interval.is_some() {
    return Err(format!(
      "{}: non-magnetic recipe {:?} has 'interval' field with no 'magnetic' to consume it",
      filename, id
    ));
  }

  let root = if recipe_value.get("root").is_some() {
    Some(parse_entity(&recipe_value["root"], type_ids, filename, &id, "root")?)
  } else {
    None
  };

  let hex = if recipe_value.get("hex").is_some() {
    Some(parse_entity(&recipe_value["hex"], type_ids, filename, &id, "hex")?)
  } else {
    None
  };

  let slots = if let Some(slots_arr) = recipe_value.get("slots").and_then(Value::as_array) {
    slots_arr
      .iter()
      .enumerate()
      .map(|(i, v)| parse_entity(v, type_ids, filename, &id, &format!("slots[{}]", i)))
      .collect::<Result<Vec<_>, _>>()?
  } else {
    Vec::new()
  };

  let reagents = if let Some(arr) = recipe_value.get("reagents").and_then(Value::as_array) {
    arr
      .iter()
      .map(|v| parse_reagent(v, filename, &id))
      .collect::<Result<Vec<_>, _>>()?
  } else {
    Vec::new()
  };

  let products = if let Some(products_obj) = recipe_value
    .get("products")
    .and_then(Value::as_object)
  {
    let mut groups: Vec<ProductGroup> = Vec::new();
    for (place_name, place_value) in products_obj {
      let place = match place_name.as_str() {
        "inventory" => ProductPlace::Inventory,
        other => {
          return Err(format!(
            "{}: recipe {:?} unknown product place {:?}, expected one of: \"inventory\"",
            filename, id, other
          ));
        }
      };
      let place_obj = place_value.as_object().ok_or_else(|| {
        format!(
          "{}: recipe {:?} products[{}] not an object (expected `{{ owner: [entities…] }}`)",
          filename, id, place_name
        )
      })?;
      for (owner_name, entities_value) in place_obj {
        let owner = match owner_name.as_str() {
          "root" => ProductOwner::Root,
          "actor" => ProductOwner::Actor,
          "hex" => ProductOwner::Hex,
          "action" => ProductOwner::Action,
          other => {
            return Err(format!(
              "{}: recipe {:?} unknown product owner {:?} under place {:?}, expected one of: \"root\", \"actor\", \"hex\", \"action\"",
              filename, id, other, place_name
            ));
          }
        };
        let entities_arr = entities_value.as_array().ok_or_else(|| {
          format!(
            "{}: recipe {:?} products[{}][{}] not an array",
            filename, id, place_name, owner_name
          )
        })?;
        let entities = entities_arr
          .iter()
          .enumerate()
          .map(|(i, v)| {
            parse_entity(
              v,
              type_ids,
              filename,
              &id,
              &format!("products[{}][{}][{}]", place_name, owner_name, i),
            )
          })
          .collect::<Result<Vec<_>, _>>()?;
        groups.push(ProductGroup {
          target: ProductTarget { place, owner },
          entities,
        });
      }
    }
    groups
  } else {
    Vec::new()
  };

  // Duration is optional only for outer magnetic recipes (where it
  // acts as the magnetic-phase loop-count cap; absent means "no
  // terminator"). For everything else the recipe's action runs in
  // `actions` for `duration` seconds, so it's required.
  let duration = if recipe_value.get("duration").is_some() {
    Some(parse_duration(&recipe_value["duration"], type_ids, filename, &id)?)
  } else {
    None
  };
  if duration.is_none() && magnetic.is_none() {
    return Err(format!(
      "{}: non-magnetic recipe {:?} missing required 'duration' field",
      filename, id
    ));
  }

  // OnCreate recipes match against the new card's def via either
  // `hex` (must be a hex-shaped card matching the entity) or `root`
  // (any card type matching the entity). At least one is required —
  // an OnCreate recipe with neither has no way to identify what it
  // fires on.
  if recipe_type == RecipeType::OnCreate && root.is_none() && hex.is_none() {
    return Err(format!(
      "{}: on_create recipe {:?} must specify either 'root' or 'hex' to identify the target card",
      filename, id
    ));
  }

  let def = RecipeDef {
    index: stable_id,
    id: id.clone(),
    recipe_type,
    root,
    hex,
    slots,
    reagents,
    products,
    duration,
    magnetic,
    interval,
  };
  Ok((id, stable_id, def))
}

/// Parse a `magnetic` field into a [`MagneticBucket`]. Same dispatch
/// logic as the top-level recipe file: bucket type ("stack" or
/// "on_create") plus direction-keyed arrays. Inner recipes are flattened
/// into `MagneticBucket.inners` in directional order.
///
/// The order matters — sub-id (the index a queued inner action carries
/// in its `flags`) is the inner's position in this flat list. Stable
/// across deploys as long as the JSON's direction keys and inner array
/// order don't change.
///
/// At most 16 inners per bucket (sub-id is 4 bits in `Action.flags`).
fn parse_magnetic_bucket(
  bucket: &serde_json::Map<String, Value>,
  filename: &str,
  parent_id: &str,
  type_ids: &BTreeMap<String, u8>,
) -> Result<MagneticBucket, String> {
  let bucket_type = bucket
    .get("type")
    .and_then(Value::as_str)
    .ok_or_else(|| format!("{}: recipe {:?} magnetic bucket missing 'type'", filename, parent_id))?;

  let direction_keys: &[(&str, RecipeType)] = match bucket_type {
    "stack" => &[
      ("up", RecipeType::Stack(StackDirection::Up)),
      ("down", RecipeType::Stack(StackDirection::Down)),
    ],
    "on_create" => &[("self", RecipeType::OnCreate)],
    other => {
      return Err(format!(
        "{}: recipe {:?} magnetic bucket has unknown type {:?}, expected \"stack\" or \"on_create\"",
        filename, parent_id, other,
      ));
    }
  };

  let mut inners: Vec<InnerRecipe> = Vec::new();
  for &(direction_key, recipe_type) in direction_keys {
    let Some(arr) = bucket.get(direction_key).and_then(Value::as_array) else {
      continue;
    };
    for (idx, inner_value) in arr.iter().enumerate() {
      let path = format!("magnetic.{}[{}]", direction_key, idx);
      inners.push(parse_inner_recipe(inner_value, recipe_type, filename, parent_id, &path, type_ids)?);
    }
  }

  if inners.len() > MAGNETIC_MAX_INNERS {
    return Err(format!(
      "{}: recipe {:?} magnetic bucket has {} inners (max {}, sub-id is 4 bits in Action.flags)",
      filename, parent_id, inners.len(), MAGNETIC_MAX_INNERS,
    ));
  }

  Ok(MagneticBucket { inners })
}

/// Hard cap on inner recipes per magnetic bucket. The queued inner
/// action stores its sub-id in 4 bits of `Action.flags`, so 16 is
/// the structural ceiling.
pub const MAGNETIC_MAX_INNERS: usize = 16;

/// Parse one inner recipe inside a magnetic bucket. Like
/// [`parse_recipe`] but: no `id`, no nested `magnetic`, no `interval`,
/// `recipe_type` is supplied by the caller from the bucket's direction
/// key. `duration` is required (it's the queued inner action's
/// duration). `path` is a JSON path fragment for error messages
/// (`"magnetic.up[0]"` etc.).
fn parse_inner_recipe(
  recipe_value: &Value,
  recipe_type: RecipeType,
  filename: &str,
  parent_id: &str,
  path: &str,
  type_ids: &BTreeMap<String, u8>,
) -> Result<InnerRecipe, String> {
  // Reject fields that don't apply to inner recipes — fail loud rather
  // than silently dropping authorial intent.
  for forbidden in ["id", "magnetic", "interval"] {
    if recipe_value.get(forbidden).is_some() {
      return Err(format!(
        "{}: recipe {:?} {}: inner recipe must not have '{}' field",
        filename, parent_id, path, forbidden
      ));
    }
  }

  let label = format!("{}/{}", parent_id, path);

  let root = if recipe_value.get("root").is_some() {
    Some(parse_entity(&recipe_value["root"], type_ids, filename, &label, "root")?)
  } else {
    None
  };

  let hex = if recipe_value.get("hex").is_some() {
    Some(parse_entity(&recipe_value["hex"], type_ids, filename, &label, "hex")?)
  } else {
    None
  };

  let slots = if let Some(slots_arr) = recipe_value.get("slots").and_then(Value::as_array) {
    slots_arr
      .iter()
      .enumerate()
      .map(|(i, v)| parse_entity(v, type_ids, filename, &label, &format!("slots[{}]", i)))
      .collect::<Result<Vec<_>, _>>()?
  } else {
    Vec::new()
  };

  let reagents = if let Some(arr) = recipe_value.get("reagents").and_then(Value::as_array) {
    arr
      .iter()
      .map(|v| parse_reagent(v, filename, &label))
      .collect::<Result<Vec<_>, _>>()?
  } else {
    Vec::new()
  };

  let products = if let Some(products_obj) = recipe_value.get("products").and_then(Value::as_object) {
    let mut groups: Vec<ProductGroup> = Vec::new();
    for (place_name, place_value) in products_obj {
      let place = match place_name.as_str() {
        "inventory" => ProductPlace::Inventory,
        other => {
          return Err(format!(
            "{}: recipe {:?} {}: unknown product place {:?}, expected \"inventory\"",
            filename, parent_id, path, other
          ));
        }
      };
      let place_obj = place_value.as_object().ok_or_else(|| {
        format!(
          "{}: recipe {:?} {}: products[{}] not an object",
          filename, parent_id, path, place_name
        )
      })?;
      for (owner_name, entities_value) in place_obj {
        let owner = match owner_name.as_str() {
          "root" => ProductOwner::Root,
          "actor" => ProductOwner::Actor,
          "hex" => ProductOwner::Hex,
          "action" => ProductOwner::Action,
          other => {
            return Err(format!(
              "{}: recipe {:?} {}: unknown product owner {:?}, expected \"root\", \"actor\", \"hex\", \"action\"",
              filename, parent_id, path, other
            ));
          }
        };
        let entities_arr = entities_value.as_array().ok_or_else(|| {
          format!(
            "{}: recipe {:?} {}: products[{}][{}] not an array",
            filename, parent_id, path, place_name, owner_name
          )
        })?;
        let entities = entities_arr
          .iter()
          .enumerate()
          .map(|(i, v)| {
            parse_entity(
              v,
              type_ids,
              filename,
              &label,
              &format!("products[{}][{}][{}]", place_name, owner_name, i),
            )
          })
          .collect::<Result<Vec<_>, _>>()?;
        groups.push(ProductGroup {
          target: ProductTarget { place, owner },
          entities,
        });
      }
    }
    groups
  } else {
    Vec::new()
  };

  // Inner duration is *required* — it becomes the queued inner action's
  // duration in `actions`. Without it the queued action would have no
  // end time.
  let duration_value = recipe_value.get("duration").ok_or_else(|| {
    format!(
      "{}: recipe {:?} {}: inner recipe missing required 'duration' field",
      filename, parent_id, path
    )
  })?;
  let duration = parse_duration(duration_value, type_ids, filename, &label)?;

  Ok(InnerRecipe {
    recipe_type,
    root,
    hex,
    slots,
    reagents,
    products,
    duration,
  })
}

/// Sentinel string parsed as `Entity::Any`. Reserved — a card with this
/// key would shadow the wildcard.
const ENTITY_ANY_LITERAL: &str = "any";
/// Prefix marking a string as `Entity::Type(<typename>)`. The remainder
/// of the string after `@` is looked up in the card-type registry at
/// recipe-build time.
const ENTITY_TYPE_PREFIX: char = '@';

/// Parse one entry from a recipe's `reagents` array. Strings `"root"`
/// and `"hex"` map to the named referents; integers `1..=255` map to
/// `Reagent::Slot`. Integer `0` is rejected with a hint to use
/// `"root"` instead — the old numeric-only encoding overloaded `0`
/// for "the chain root", and we want load-time errors when a recipe
/// file is on the old format.
fn parse_reagent(value: &Value, filename: &str, recipe_id: &str) -> Result<Reagent, String> {
  if let Some(s) = value.as_str() {
    return match s {
      "root" => Ok(Reagent::Root),
      "hex" => Ok(Reagent::Hex),
      other => Err(format!(
        "{}: recipe {:?} reagent string {:?} unknown — expected \"root\" or \"hex\"",
        filename, recipe_id, other
      )),
    };
  }
  if let Some(n) = value.as_u64() {
    if n == 0 {
      return Err(format!(
        "{}: recipe {:?} reagent index 0 not allowed — use \"root\" to consume the chain root",
        filename, recipe_id
      ));
    }
    if n > u8::MAX as u64 {
      return Err(format!(
        "{}: recipe {:?} reagent slot index {} exceeds u8 max",
        filename, recipe_id, n
      ));
    }
    return Ok(Reagent::Slot(n as u8));
  }
  Err(format!(
    "{}: recipe {:?} reagent {:?} not a string or non-negative integer",
    filename, recipe_id, value
  ))
}

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
