// packing.rs

// ---- Zone / World helpers (used by actions.rs, zones.rs, bootstrap.rs) ----

pub fn world_to_zone(q: i32, r: i32) -> (i16, i16) {
  (q.div_euclid(8) as i16, r.div_euclid(8) as i16)
}

pub fn world_to_position(q: i32, r: i32) -> (u8, u8) {
  (q.rem_euclid(8) as u8, r.rem_euclid(8) as u8)
}

pub fn zone_to_world(zone_q: i16, zone_r: i16, q: u8, r: u8) -> (i32, i32) {
  (zone_q as i32 * 8 + q as i32, zone_r as i32 * 8 + r as i32)
}

// ---- macro_location (u64) ----
// surface=1 (world):  [ zone_q: i16 ][ zone_r: i16 ][ reserved: u16 ][ layer: u8 ][ 1: u8 ]
// surface=2 (panel):  [ card_id: u32 ][ reserved: u16 ][ layer: u8 ][ 2: u8 ]

pub fn pack_macro_world(zone_q: i16, zone_r: i16, layer: u8) -> u64 {
  ((zone_q as u16 as u64) << 48)
    | ((zone_r as u16 as u64) << 32)
    | ((layer as u64) << 8)
    | 1u64
}

pub fn pack_macro_panel(card_id: u32, layer: u8) -> u64 {
  ((card_id as u64) << 32) | ((layer as u64) << 8) | 2u64
}

pub fn surface_from_macro(loc: u64) -> u8  { (loc & 0xFF) as u8 }
pub fn layer_from_macro(loc: u64) -> u8    { ((loc >> 8) & 0xFF) as u8 }
pub fn zone_q_from_macro(loc: u64) -> i16  { (loc >> 48) as u16 as i16 }
pub fn zone_r_from_macro(loc: u64) -> i16  { ((loc >> 32) & 0xFFFF) as u16 as i16 }
pub fn card_id_from_macro(loc: u64) -> u32 { (loc >> 32) as u32 }

// ---- micro_location (u32) variants ----

// Stacked: full u32 = stacked_id (the card this one is stacked onto)
pub fn pack_micro_stacked(stacked_id: u32) -> u32 { stacked_id }
pub fn unpack_micro_stacked(micro: u32) -> u32 { micro }

// Local hex: [ local_q: u4 ][ local_r: u4 ][ reserved: u24 ]
pub fn pack_micro_hex(local_q: u8, local_r: u8) -> u32 {
  ((local_q as u32 & 0xF) << 28) | ((local_r as u32 & 0xF) << 24)
}

pub fn unpack_micro_hex(micro: u32) -> (u8, u8) {
  (((micro >> 28) & 0xF) as u8, ((micro >> 24) & 0xF) as u8)
}

// Local pixel: [ local_x: i16 ][ local_y: i16 ]
pub fn pack_micro_pixel(local_x: i16, local_y: i16) -> u32 {
  ((local_x as u16 as u32) << 16) | (local_y as u16 as u32)
}

pub fn unpack_micro_pixel(micro: u32) -> (i16, i16) {
  ((micro >> 16) as u16 as i16, (micro & 0xFFFF) as u16 as i16)
}

// ---- packed_definition (u16) ----
// [ card_type: u4 ][ category: u4 ][ definition_id: u8 ]

pub fn pack_definition(card_type: u8, category: u8, definition_id: u8) -> u16 {
  (((card_type as u16) & 0xF) << 12)
    | (((category as u16) & 0xF) << 8)
    | (definition_id as u16)
}

pub fn card_type_from_definition(def: u16) -> u8     { ((def >> 12) & 0xF) as u8 }
pub fn category_from_definition(def: u16) -> u8      { ((def >> 8)  & 0xF) as u8 }
pub fn definition_id_from_definition(def: u16) -> u8 {  (def & 0xFF)       as u8 }

// ---- card flags (u16) ----

// Stacking direction flags — set on children, not on the root.
// micro_location holds the parent card_id when either is set.
// STACKED_UP:   card is above its parent (renders upward from root).
// STACKED_DOWN: card is below its parent (renders downward from root).
// Invariant: a STACKED_DOWN card may not receive STACKED_UP children,
//            and a STACKED_UP card may not receive STACKED_DOWN children.
pub const CARD_FLAG_STACKED_UP:      u16 = 1 << 0;
pub const CARD_FLAG_STACKED_DOWN:    u16 = 1 << 1;
pub const CARD_FLAG_STACKABLE:       u16 = 1 << 2;
pub const CARD_FLAG_POSITION_LOCKED: u16 = 1 << 3;
pub const CARD_FLAG_POSITION_HOLD:   u16 = 1 << 4;
