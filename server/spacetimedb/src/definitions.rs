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

  let mut by_packed: BTreeMap<u16, CardDefinition> = BTreeMap::new();
  let mut by_path: BTreeMap<(u8, u8, String), u16> = BTreeMap::new();

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

      for (idx, (key, value)) in cards_obj.iter().enumerate() {
        // 1-indexed; 0 reserved as sentinel.
        let definition_id_idx = idx + 1;
        if definition_id_idx > u8::MAX as usize {
          return Err(format!(
            "{}: bucket {}/{}: more than {} cards (definition_id overflow)",
            filename,
            type_name,
            category_name,
            u8::MAX,
          ));
        }
        let definition_id = definition_id_idx as u8;
        let definition = parse_card(filename, value, card_type, card_category, definition_id, key)?;
        let packed = pack_definition(card_type, card_category, definition_id);
        by_packed.insert(packed, definition);
        by_path.insert((card_type, card_category, key.clone()), packed);
      }
    }
  }

  Ok(CardRegistry { by_packed, by_path, type_ids, category_ids })
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
