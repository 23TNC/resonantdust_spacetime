//! Bit-packing helpers for the cards / zones tables.
//!
//! - [`pack_definition`] / [`unpack_definition`] — `Card.packed_definition`
//!   (`[card_type:u4][card_category:u4][definition_id:u8]`).
//! - [`pack_recipe`] / [`unpack_recipe`] — `Action.recipe` and
//!   `MagneticAction.recipe`
//!   (`[recipe_type:u3][recipe_category:u3][recipe_id:u10]`). Type and
//!   category ids come from `recipe_types.json`; recipe id is the value
//!   from `recipes/id.json` (nested under type/category).
//! - [`pack_world_macro_zone`] / [`unpack_world_macro_zone`] — world-layer
//!   `Card.macro_zone` and `Zone.macro_zone`. For inventory cards the same
//!   field stores the holder's `player_id` directly and doesn't go through
//!   this packing.
//!
//! `micro_zone` and `micro_location` packing live in their respective
//! modules ([`crate::zones::LocalCoord`] for `micro_zone` (q, r) coords)
//! or are still pending the rest of the world-board work.

// ---------- packed_definition (u16) ----------
// bits 15..12 card_type (u4), bits 11..8 card_category (u4), bits 7..0 definition_id (u8).

const DEFINITION_NIBBLE_MASK: u16 = 0b1111;

#[inline]
pub fn pack_definition(card_type: u8, card_category: u8, definition_id: u8) -> u16 {
  debug_assert!(card_type as u16 <= DEFINITION_NIBBLE_MASK, "card_type exceeds 4 bits");
  debug_assert!(card_category as u16 <= DEFINITION_NIBBLE_MASK, "card_category exceeds 4 bits");
  ((card_type as u16 & DEFINITION_NIBBLE_MASK) << 12)
    | ((card_category as u16 & DEFINITION_NIBBLE_MASK) << 8)
    | (definition_id as u16)
}

#[inline]
pub fn unpack_definition(packed: u16) -> (u8, u8, u8) {
  let card_type = ((packed >> 12) & DEFINITION_NIBBLE_MASK) as u8;
  let card_category = ((packed >> 8) & DEFINITION_NIBBLE_MASK) as u8;
  let definition_id = packed as u8;
  (card_type, card_category, definition_id)
}

// ---------- packed recipe (u16) ----------
// bits 15..13 recipe_type     (u3, 0..=7)
// bits 12..10 recipe_category (u3, 0..=7)
// bits  9..0  recipe_id       (u10, 0..=1023)

/// Mask covering the low 10 bits — the maximum value of a recipe id
/// stored in `recipes/id.json` (one entry per type+category bucket).
pub const RECIPE_ID_MASK: u16 = 0x03FF;
/// Mask covering 3 bits — the maximum value of a recipe type or
/// category id (`recipe_types.json` types and categories).
pub const RECIPE_TYPE_OR_CATEGORY_MASK: u16 = 0b111;

#[inline]
pub fn pack_recipe(recipe_type: u8, recipe_category: u8, recipe_id: u16) -> u16 {
  debug_assert!(
    recipe_type as u16 <= RECIPE_TYPE_OR_CATEGORY_MASK,
    "recipe_type exceeds 3 bits"
  );
  debug_assert!(
    recipe_category as u16 <= RECIPE_TYPE_OR_CATEGORY_MASK,
    "recipe_category exceeds 3 bits"
  );
  debug_assert!(recipe_id <= RECIPE_ID_MASK, "recipe_id exceeds 10 bits");
  ((recipe_type as u16 & RECIPE_TYPE_OR_CATEGORY_MASK) << 13)
    | ((recipe_category as u16 & RECIPE_TYPE_OR_CATEGORY_MASK) << 10)
    | (recipe_id & RECIPE_ID_MASK)
}

#[inline]
pub fn unpack_recipe(packed: u16) -> (u8, u8, u16) {
  let recipe_type = ((packed >> 13) & RECIPE_TYPE_OR_CATEGORY_MASK) as u8;
  let recipe_category = ((packed >> 10) & RECIPE_TYPE_OR_CATEGORY_MASK) as u8;
  let recipe_id = packed & RECIPE_ID_MASK;
  (recipe_type, recipe_category, recipe_id)
}

// ---------- macro_zone (u32) for world cells ----------
// World layer:
//   bits 31..16 = zone_q (i16, two's-complement)
//   bits 15..0  = zone_r (i16, two's-complement)
//
// Inventory layer (`Card.layer == LAYER_INVENTORY`):
//   the field stores the inventory holder's `player_id` directly.
//   No packing — `macro_zone == player_id`.
//
// Layer is *not* baked into `macro_zone`; it's tracked separately as
// `Card.layer` (and on the Zone side, multi-layer support is forward-
// looking, so today every Zone row implicitly lives on the single
// active world layer).

/// Pack zone-level axial coordinates `(zone_q, zone_r)` into the
/// `u32 macro_zone` field used by world cards and the zones table.
#[inline]
pub fn pack_world_macro_zone(zone_q: i16, zone_r: i16) -> u32 {
  ((zone_q as u16 as u32) << 16) | (zone_r as u16 as u32)
}

/// Inverse of [`pack_world_macro_zone`]. Always succeeds — every `u32`
/// is a valid (zone_q, zone_r) pair.
#[inline]
pub fn unpack_world_macro_zone(macro_zone: u32) -> (i16, i16) {
  let zone_q = (macro_zone >> 16) as u16 as i16;
  let zone_r = macro_zone as u16 as i16;
  (zone_q, zone_r)
}

/// Pack pixel coordinates `(x, y)` into the `u32 micro_location` field
/// used by loose world cards (`stack_state == STACK_STATE_LOOSE`).
/// Same two's-complement layout as [`pack_world_macro_zone`]:
/// bits 31..16 = x, bits 15..0 = y.
#[inline]
pub fn pack_world_micro_location(x: i16, y: i16) -> u32 {
  ((x as u16 as u32) << 16) | (y as u16 as u32)
}

/// Inverse of [`pack_world_micro_location`].
#[inline]
pub fn unpack_world_micro_location(micro_location: u32) -> (i16, i16) {
  let x = (micro_location >> 16) as u16 as i16;
  let y = micro_location as u16 as i16;
  (x, y)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn definition_roundtrip() {
    for card_type in 0..=0b1111u8 {
      for card_category in 0..=0b1111u8 {
        for definition_id in [0u8, 1, 42, 200, 255] {
          let packed = pack_definition(card_type, card_category, definition_id);
          assert_eq!(unpack_definition(packed), (card_type, card_category, definition_id));
        }
      }
    }
  }

  #[test]
  fn recipe_roundtrip() {
    for recipe_type in 0..=0b111u8 {
      for recipe_category in 0..=0b111u8 {
        for recipe_id in [0u16, 1, 7, 42, 511, 1023] {
          let packed = pack_recipe(recipe_type, recipe_category, recipe_id);
          assert_eq!(unpack_recipe(packed), (recipe_type, recipe_category, recipe_id));
        }
      }
    }
  }

  #[test]
  fn world_macro_zone_roundtrip() {
    for q in [i16::MIN, -1024, -1, 0, 1, 1024, i16::MAX] {
      for r in [i16::MIN, -1024, -1, 0, 1, 1024, i16::MAX] {
        let packed = pack_world_macro_zone(q, r);
        assert_eq!(unpack_world_macro_zone(packed), (q, r));
      }
    }
  }
}
