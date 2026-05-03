//! Bit-packing helpers for the cards table.
//!
//! Currently only `packed_definition` is packed. The other packing helpers
//! (`macro_zone` variants, `micro_zone`, `micro_location`) were removed
//! along with their fields when the table was cut down to the inventory
//! POC. They'll come back with the world board.

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
}
