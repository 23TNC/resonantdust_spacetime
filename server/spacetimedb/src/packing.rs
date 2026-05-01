// packing.rs — bit-packed location and definition helpers.
//
// Phase 5 schema:
//
//   layer:          u8   discriminates panel (0..31) vs world (32..255)
//   macro_zone:     u32
//                     panel:  full u32 = soul_card_id
//                     world:  [zone_q:i16][zone_r:i16]
//   micro_zone:     u8   [local_q:u3][local_r:u3][unused:u2]
//   micro_location: u32  variant per stack_state (in flags bits 6-7):
//                     00 root, loose       [pixel_x:i16][pixel_y:i16]
//                     01 stack_up          parent (rect) card_id
//                     10 stack_down        parent (rect) card_id
//                     11 root, attached    hex card_id, or 0 = floor at own hex
//
// Stacking direction is no longer in `flags`; it's encoded by the
// STACK_STATE bits (see CARD_FLAG_STACK_STATE_MASK below) which were
// reclaimed from the previous STACKED_UP / STACKED_DOWN bit positions.

use crate::definitions::CardShape;

// ─── Layer split ──────────────────────────────────────────────────────────────

/// Layer values strictly less than this are panel layers; ≥ this are world.
pub const PANEL_LAYER_MAX: u8 = 32;

/// True iff `layer` denotes a panel layer.
#[inline]
pub fn is_panel_layer(layer: u8) -> bool { layer < PANEL_LAYER_MAX }

/// True iff `layer` denotes a world layer.
#[inline]
pub fn is_world_layer(layer: u8) -> bool { layer >= PANEL_LAYER_MAX }

/// Default panel layer for a player's primary inventory.  Reserved low-range
/// id; future panels (trade window, scratch panel) will pick other ids in
/// 0..PANEL_LAYER_MAX.
pub const PANEL_LAYER_INVENTORY: u8 = 1;

/// Default world layer for the ground.  Reserved id; future world layers
/// (sky, underground) pick from PANEL_LAYER_MAX..255.
pub const WORLD_LAYER_GROUND: u8 = 32;

// ─── Zone math ────────────────────────────────────────────────────────────────

pub const ZONE_SIZE: i32 = 8;

pub fn world_to_zone(q: i32, r: i32) -> (i16, i16) {
  (q.div_euclid(ZONE_SIZE) as i16, r.div_euclid(ZONE_SIZE) as i16)
}

pub fn world_to_position(q: i32, r: i32) -> (u8, u8) {
  (q.rem_euclid(ZONE_SIZE) as u8, r.rem_euclid(ZONE_SIZE) as u8)
}

pub fn zone_to_world(zone_q: i16, zone_r: i16, q: u8, r: u8) -> (i32, i32) {
  (zone_q as i32 * ZONE_SIZE + q as i32, zone_r as i32 * ZONE_SIZE + r as i32)
}

// ─── macro_zone (u32) ─────────────────────────────────────────────────────────
//
// Panel: full u32 = soul_card_id
// World: [zone_q:i16][zone_r:i16] — top 16 bits q, bottom 16 r

pub fn pack_macro_world(zone_q: i16, zone_r: i16) -> u32 {
  ((zone_q as u16 as u32) << 16) | (zone_r as u16 as u32)
}

pub fn pack_macro_panel(soul_card_id: u32) -> u32 {
  soul_card_id
}

/// Decode (zone_q, zone_r) from a world `macro_zone`.  Caller is responsible
/// for verifying `is_world_layer(layer)` first; on a panel layer the field
/// is a soul_card_id and these accessors are nonsense.
pub fn zone_q_from_macro(macro_zone: u32) -> i16 { (macro_zone >> 16) as u16 as i16 }
pub fn zone_r_from_macro(macro_zone: u32) -> i16 { (macro_zone & 0xFFFF) as u16 as i16 }

/// Decode a `macro_zone` for a panel layer back to its anchoring soul_id.
#[inline]
pub fn soul_id_from_macro(macro_zone: u32) -> u32 { macro_zone }

// ─── micro_zone (u8) ──────────────────────────────────────────────────────────
//
// [local_q:u3][local_r:u3][unused:u2]
//
// `local_q` and `local_r` are in-zone coordinates 0..7 for world cards; they
// are 0/0 for cards in panels.  When a card is stacked or attached, these
// fields mirror the anchor's coordinates so subscription `WHERE macro_zone =
// ...` returns the chain along with the anchor.

pub fn pack_micro_zone(local_q: u8, local_r: u8) -> u8 {
  ((local_q & 0x07) << 5) | ((local_r & 0x07) << 2)
}

pub fn local_q_from_micro_zone(mz: u8) -> u8 { (mz >> 5) & 0x07 }
pub fn local_r_from_micro_zone(mz: u8) -> u8 { (mz >> 2) & 0x07 }

// ─── micro_location (u32) variants by stack_state ─────────────────────────────

/// Sentinel value for stack_state == 11 (root attached) meaning "attached to
/// whatever hex card sits at my own (macro_zone, micro_zone)" — resolved via
/// zone packed-tile data plus any materialized override card row at the same
/// position.  Never a valid card_id.
pub const MICRO_ATTACHED_TO_FLOOR: u32 = 0;

/// Encode pixel offset for a loose root (stack_state == 00).  Cosmetic only.
pub fn pack_micro_pixel(pixel_x: i16, pixel_y: i16) -> u32 {
  ((pixel_x as u16 as u32) << 16) | (pixel_y as u16 as u32)
}

pub fn unpack_micro_pixel(micro: u32) -> (i16, i16) {
  ((micro >> 16) as u16 as i16, (micro & 0xFFFF) as u16 as i16)
}

/// Encode a stacked card's parent (stack_state in {01, 10}).
pub fn pack_micro_parent(parent_card_id: u32) -> u32 { parent_card_id }

/// Encode an attached root (stack_state == 11) with an explicit hex anchor.
/// Pass `MICRO_ATTACHED_TO_FLOOR` (0) for "attach to whatever hex card is at
/// my own position."
pub fn pack_micro_attached(hex_card_id: u32) -> u32 { hex_card_id }

// ─── packed_definition (u16) ──────────────────────────────────────────────────
// [card_type: u4 ][ category: u4 ][ definition_id: u8 ]

pub fn pack_definition(card_type: u8, category: u8, definition_id: u8) -> u16 {
  (((card_type as u16) & 0xF) << 12)
    | (((category as u16) & 0xF) << 8)
    | (definition_id as u16)
}

pub fn card_type_from_definition(def: u16) -> u8     { ((def >> 12) & 0xF) as u8 }
#[allow(dead_code)]
pub fn category_from_definition(def: u16) -> u8      { ((def >> 8)  & 0xF) as u8 }
pub fn definition_id_from_definition(def: u16) -> u8 {  (def & 0xFF)       as u8 }

// ─── flags (u16) ──────────────────────────────────────────────────────────────
//
// Bit map:
//   0  STACKABLE         — card can have other cards stacked onto it
//   1  POSITION_LOCKED   — player cannot move this card (permanent lock)
//   2  POSITION_HOLD     — temporarily locked (mid-server-action)
//   3  SLOT_HOLD         — claimed by a running action's slots; matcher
//                          excludes from new recipes
//   4  reserved
//   5  reserved
//   6-7  STACK_STATE     — 2-bit field encoding the card's positional role:
//                            00 root, loose (own position via macro/micro_zone +
//                                            cosmetic pixel offset in micro_location)
//                            01 stack_up    (above its parent rect)
//                            10 stack_down  (below its parent rect)
//                            11 root, attached (anchored to a hex card; see
//                                              micro_location encoding)
//   8-15 reserved
//
// Old STACKED_UP / STACKED_DOWN flag bits (formerly bits 0/1) are gone.  Use
// `stack_state(flags)` to read state and `with_stack_state(flags, …)` to set.

pub const CARD_FLAG_STACKABLE:       u16 = 1 << 0;
pub const CARD_FLAG_POSITION_LOCKED: u16 = 1 << 1;
pub const CARD_FLAG_POSITION_HOLD:   u16 = 1 << 2;
pub const CARD_FLAG_SLOT_HOLD:       u16 = 1 << 3;

pub const CARD_FLAG_STACK_STATE_MASK:  u16 = 0b11 << 6;
pub const CARD_FLAG_STACK_STATE_SHIFT: u32 = 6;

pub const STACK_STATE_LOOSE:    u8 = 0b00;
pub const STACK_STATE_UP:       u8 = 0b01;
pub const STACK_STATE_DOWN:     u8 = 0b10;
pub const STACK_STATE_ATTACHED: u8 = 0b11;

#[inline]
pub fn stack_state(flags: u16) -> u8 {
  ((flags & CARD_FLAG_STACK_STATE_MASK) >> CARD_FLAG_STACK_STATE_SHIFT) as u8
}

#[inline]
pub fn with_stack_state(flags: u16, state: u8) -> u16 {
  let cleared = flags & !CARD_FLAG_STACK_STATE_MASK;
  cleared | (((state as u16) & 0b11) << CARD_FLAG_STACK_STATE_SHIFT)
}

#[inline]
pub fn is_stacked(flags: u16) -> bool {
  matches!(stack_state(flags), STACK_STATE_UP | STACK_STATE_DOWN)
}

#[inline]
pub fn is_root(flags: u16) -> bool {
  matches!(stack_state(flags), STACK_STATE_LOOSE | STACK_STATE_ATTACHED)
}

// ─── Hex/Rect classification helpers (re-exports for ergonomic access) ────────

/// True iff `card_type`'s shape is `hex` (per `data/card_types.json`).
#[allow(dead_code)]
pub fn is_hex_card(card_type: u8) -> bool {
  matches!(crate::definitions::card_shape(card_type), Some(CardShape::Hex))
}

/// True iff `card_type`'s shape is `rect` (per `data/card_types.json`).
#[allow(dead_code)]
pub fn is_rect_card(card_type: u8) -> bool {
  matches!(crate::definitions::card_shape(card_type), Some(CardShape::Rect))
}
