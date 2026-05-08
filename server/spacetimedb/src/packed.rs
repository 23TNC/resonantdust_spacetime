// Pack/unpack helpers for the bit-packed columns on the cards table.
//
// Layouts:
//   valid_at         u64 = [card_id: u32 | time_secs: u32]   (high | low)
//   macro_zone       u32 = [q: i16 | r: i16]                 (high | low)
//   micro_zone       u8  = [q: u3 | r: u3 | stacked_state: u2]
//   micro_location   u32 = card_id (when stacked) | [x: i16 | y: i16] (when free)
//   packed_def       u16 = [card_type: u4 | card_category: u4 | def_id: u8]
//   zone_def         u8  = [card_type: u4 | card_category: u4]
//   tile row         u64 = 8 little-endian u8 def_ids (byte i = column i)
//   recipe           u16 = [recipe_type: u3 | recipe_category: u3 | recipe_id: u10]

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackedState {
    Free = 0,
    OnCard = 1,
    Reserved2 = 2,
    Reserved3 = 3,
}

impl StackedState {
    pub fn from_u2(v: u8) -> Self {
        match v & 0b11 {
            0 => Self::Free,
            1 => Self::OnCard,
            2 => Self::Reserved2,
            _ => Self::Reserved3,
        }
    }

    pub fn to_u2(self) -> u8 {
        self as u8
    }
}

// ---- valid_at ----------------------------------------------------------

pub fn pack_valid_at(card_id: u32, time_secs: u32) -> u64 {
    ((card_id as u64) << 32) | (time_secs as u64)
}

pub fn unpack_valid_at(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

pub fn valid_at_card_id(v: u64) -> u32 {
    (v >> 32) as u32
}

pub fn valid_at_time(v: u64) -> u32 {
    v as u32
}

// ---- macro_zone --------------------------------------------------------

pub fn pack_macro_zone(q: i16, r: i16) -> u32 {
    ((q as u16 as u32) << 16) | (r as u16 as u32)
}

pub fn unpack_macro_zone(v: u32) -> (i16, i16) {
    ((v >> 16) as u16 as i16, v as u16 as i16)
}

// ---- micro_zone --------------------------------------------------------

pub fn pack_micro_zone(q: u8, r: u8, state: StackedState) -> u8 {
    ((q & 0b111) << 5) | ((r & 0b111) << 2) | state.to_u2()
}

pub fn unpack_micro_zone(v: u8) -> (u8, u8, StackedState) {
    (
        (v >> 5) & 0b111,
        (v >> 2) & 0b111,
        StackedState::from_u2(v),
    )
}

pub fn micro_zone_state(v: u8) -> StackedState {
    StackedState::from_u2(v)
}

// ---- micro_location ----------------------------------------------------

pub fn pack_micro_location_xy(x: i16, y: i16) -> u32 {
    ((x as u16 as u32) << 16) | (y as u16 as u32)
}

pub fn unpack_micro_location_xy(v: u32) -> (i16, i16) {
    ((v >> 16) as u16 as i16, v as u16 as i16)
}

pub fn pack_micro_location_card_id(card_id: u32) -> u32 {
    card_id
}

pub fn unpack_micro_location_card_id(v: u32) -> u32 {
    v
}

// ---- packed_definition -------------------------------------------------

pub fn pack_definition(card_type: u8, card_category: u8, def_id: u8) -> u16 {
    (((card_type & 0xF) as u16) << 12)
        | (((card_category & 0xF) as u16) << 8)
        | (def_id as u16)
}

pub fn unpack_definition(v: u16) -> (u8, u8, u8) {
    (
        ((v >> 12) & 0xF) as u8,
        ((v >> 8) & 0xF) as u8,
        (v & 0xFF) as u8,
    )
}

// ---- zone_definition (u8 = u4 card_type | u4 card_category) -----------

pub fn pack_zone_definition(card_type: u8, card_category: u8) -> u8 {
    ((card_type & 0xF) << 4) | (card_category & 0xF)
}

pub fn unpack_zone_definition(v: u8) -> (u8, u8) {
    ((v >> 4) & 0xF, v & 0xF)
}

// ---- recipe (u16 = u3 type | u3 category | u10 id) -------------------

pub const RECIPE_TYPE_OR_CATEGORY_MASK: u16 = 0x7;
pub const RECIPE_ID_MASK: u16 = 0x3FF;

pub fn pack_recipe(recipe_type: u8, recipe_category: u8, recipe_id: u16) -> u16 {
    (((recipe_type as u16) & RECIPE_TYPE_OR_CATEGORY_MASK) << 13)
        | (((recipe_category as u16) & RECIPE_TYPE_OR_CATEGORY_MASK) << 10)
        | (recipe_id & RECIPE_ID_MASK)
}

pub fn unpack_recipe(v: u16) -> (u8, u8, u16) {
    (
        ((v >> 13) & RECIPE_TYPE_OR_CATEGORY_MASK) as u8,
        ((v >> 10) & RECIPE_TYPE_OR_CATEGORY_MASK) as u8,
        v & RECIPE_ID_MASK,
    )
}

// ---- tile rows (u64 holds 8 u8 def_ids, little-endian) ----------------

pub fn pack_tiles(tiles: [u8; 8]) -> u64 {
    u64::from_le_bytes(tiles)
}

pub fn unpack_tiles(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

pub fn tile_byte(v: u64, idx: usize) -> u8 {
    v.to_le_bytes()[idx]
}

pub fn with_tile_byte(v: u64, idx: usize, b: u8) -> u64 {
    let mut bs = v.to_le_bytes();
    bs[idx] = b;
    u64::from_le_bytes(bs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_at_roundtrip() {
        let v = pack_valid_at(0xDEAD_BEEF, 0x1234_5678);
        assert_eq!(unpack_valid_at(v), (0xDEAD_BEEF, 0x1234_5678));
        assert_eq!(valid_at_card_id(v), 0xDEAD_BEEF);
        assert_eq!(valid_at_time(v), 0x1234_5678);
    }

    #[test]
    fn macro_zone_signed_roundtrip() {
        let v = pack_macro_zone(-1, 1);
        assert_eq!(unpack_macro_zone(v), (-1, 1));
        let v = pack_macro_zone(i16::MIN, i16::MAX);
        assert_eq!(unpack_macro_zone(v), (i16::MIN, i16::MAX));
    }

    #[test]
    fn micro_zone_roundtrip() {
        let v = pack_micro_zone(5, 3, StackedState::OnCard);
        assert_eq!(unpack_micro_zone(v), (5, 3, StackedState::OnCard));
    }

    #[test]
    fn micro_location_xy_signed() {
        let v = pack_micro_location_xy(-100, 200);
        assert_eq!(unpack_micro_location_xy(v), (-100, 200));
    }

    #[test]
    fn definition_roundtrip() {
        let v = pack_definition(0xA, 0x5, 0xC3);
        assert_eq!(unpack_definition(v), (0xA, 0x5, 0xC3));
    }

    #[test]
    fn recipe_roundtrip() {
        let v = pack_recipe(0b101, 0b011, 0x2F4);
        assert_eq!(unpack_recipe(v), (0b101, 0b011, 0x2F4));
        // Saturate each field at its mask.
        let v = pack_recipe(0x7, 0x7, RECIPE_ID_MASK);
        assert_eq!(unpack_recipe(v), (0x7, 0x7, RECIPE_ID_MASK));
        // Overflow bits in inputs should be masked off cleanly.
        let v = pack_recipe(0xFF, 0xFF, 0xFFFF);
        assert_eq!(unpack_recipe(v), (0x7, 0x7, RECIPE_ID_MASK));
    }

    #[test]
    fn zone_definition_roundtrip() {
        let v = pack_zone_definition(0xC, 0x3);
        assert_eq!(unpack_zone_definition(v), (0xC, 0x3));
    }

    #[test]
    fn tile_roundtrip() {
        let row = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let v = pack_tiles(row);
        assert_eq!(unpack_tiles(v), row);
        assert_eq!(tile_byte(v, 0), 1);
        assert_eq!(tile_byte(v, 7), 8);
        let v2 = with_tile_byte(v, 3, 99);
        assert_eq!(tile_byte(v2, 3), 99);
        assert_eq!(tile_byte(v2, 0), 1);
    }
}
