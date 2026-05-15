// Pack/unpack helpers for the bit-packed columns on the cards table.
//
// Layouts:
//   valid_at         u64 = [time_ms: u48 | sequence: u16]                   (high | low)
//                          — see `pack_valid_at` below and `sequence.rs`
//                          for how the u16 disambiguator is allocated.
//   macro_zone       u32 = [q: i16 | r: i16]                                (high | low)
//   micro_zone       u8  — TWO INTERPRETATIONS, gated by (stacked_state, surface):
//     stack layout — state == OnRoot AND surface < 64:
//                   u8 = [position: u5 | direction: u1 | stacked_state: u2]
//     legacy layout — every other case (Free / ReservedSlot / OnHex / world):
//                   u8 = [q: u3 | r: u3 | stacked_state: u2]
//   micro_location   u32 — interpretation depends on stacked_state:
//     Free          → [x: i16 | y: i16] (loose XY in surface-local coords)
//     ReservedSlot  → RESERVED for an upcoming slot-pinning mode where
//                     micro_location will hold the immediate parent's
//                     card_id (not the root). Unused today.
//     OnRoot        → ROOT card_id of the rect chain (chain order /
//                     direction from `micro_zone.position` and
//                     `micro_zone.direction`)
//     OnHex         → parent hex card_id (walk up via `micro_location`
//                     to find root hex; hex chains aren't migrated)
//   packed_def       u16 = [card_type: u4 | card_category: u4 | def_id: u8]
//   zone_def         u8  = [card_type: u4 | card_category: u4]
//   tile row         u64 = 8 little-endian u8 def_ids (byte i = column i)
//   recipe           u16 = [recipe_type: u3 | recipe_category: u3 | recipe_id: u10]
//
// Rect chains (state = OnRoot) use the (root_id, position, direction)
// model; hex chains (OnHex) keep parent-pointer walking. Rect-on-hex (a
// rect card with state=OnHex) is a leaf — no rect chain hangs off it.
// The "server is forcing this position" signal moved out of micro_zone
// (it used to live in bit 2 alongside position/state) and now lives in
// `flags` (`force_position` at bit 11) — see `cards/flags.json`.

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackedState {
    /// Loose. `micro_location` is encoded `(x, y)`; `micro_zone` upper
    /// 6 bits are the legacy `(q, r)`.
    Free = 0,
    /// Parent-pointer slot. `micro_location` holds the **immediate
    /// parent's** card_id (which can itself be `Slot`, `OnRoot`,
    /// `Free`, or `OnHex`), not the chain root. The slot's direction
    /// (bit 2 of `micro_zone`) is stored explicitly even though it
    /// could be derived from the parent — saves a chain walk on every
    /// render / parent-resolve. Position from root is implicit: walk
    /// `micro_location` up until you hit a state that's not `Slot`
    /// (for rect chains, that'll be `OnRoot` or `Free`). Used by
    /// `propose_action` to stitch recipe slots above the actor —
    /// chain integrity follows the actor when the recipe doesn't pin
    /// to a root.
    Slot = 1,
    /// Stacked on a rect chain root — `micro_location` is the **root**
    /// card_id (not the immediate parent). Chain order comes from
    /// `micro_zone.position`; chain direction (top vs bottom of root)
    /// comes from `micro_zone.direction`.
    OnRoot = 2,
    /// Stacked on a hex. Walks the chain via `micro_location = parent_hex_id`
    /// until the root hex (state=Free). Legacy layout still applies; hex
    /// chains aren't migrated to (root_id, position).
    OnHex = 3,
}

impl StackedState {
    pub fn from_u2(v: u8) -> Self {
        match v & 0b11 {
            0 => Self::Free,
            1 => Self::Slot,
            2 => Self::OnRoot,
            _ => Self::OnHex,
        }
    }

    pub fn to_u2(self) -> u8 {
        self as u8
    }
}

// ---- surface bands ------------------------------------------------------
//
// `Card.surface` and `Zone.surface` are u8, but the values are
// banded into ranges with different semantics. Every band is a
// "container kind" — the `(surface, macro_zone)` tuple identifies
// which container a row belongs to. `macro_zone` means different
// things in different bands:
//
// - INVENTORY_LAYER (1):           `macro_zone` = the owning soul's
//                                  `card_id`. The player's hand /
//                                  bag / inventory grid.
// - POCKET_DIMENSION_LAYER (32):   `macro_zone` = the anchor card's
//                                  `card_id`. A private interior
//                                  carried by an anchor card.
// - MINI_ZONE_LAYER (63):          `macro_zone` = the anchor card's
//                                  `card_id`. A radius-3 hex disk
//                                  overlaying the world wherever
//                                  the anchor is placed. The anchor
//                                  itself lives at WORLD_LAYER.
// - WORLD_LAYER (64) and above:    `macro_zone` = packed
//                                  `(chunkQ:i16, chunkR:i16)`. The
//                                  shared world hex grid.
//
// The split at `< WORLD_LAYER` is what existing code keys "stack
// layout" rules and inventory-like behavior off. The split at
// `>= WORLD_LAYER` is what world-vs-personal queries key off. The
// new MINI_ZONE_LAYER sits just below WORLD_LAYER intentionally:
// stack-layout rules apply (cards on mini_zone tiles can chain with
// the existing rect-stack machinery), and world-only queries
// continue to skip mini_zone contents.
pub const INVENTORY_LAYER: u8 = 1;
pub const POCKET_DIMENSION_LAYER: u8 = 32;
pub const MINI_ZONE_LAYER: u8 = 63;
pub const WORLD_LAYER: u8 = 64;

// ---- valid_at ----------------------------------------------------------
//
// PK layout: `(time_ms_u48 << 16) | sequence_u16`.
//
// - High 48 bits: milliseconds since Unix epoch. u48 ms ≈ 8920 years
//   of runway from epoch (covers our lifetime trivially). PK ordering
//   is chronological — a btree scan walks rows in time order, useful
//   for any range queries that need it (though most callers go via
//   `card_id` btree index, where the explicit `max_by_key` ordering
//   is what's load-bearing).
// - Low 16 bits: global sequence number from `sequence::next_sequence`,
//   refreshed per write. Disambiguates two writes that share a
//   millisecond — within one module, even thousands of same-ms writes
//   sit far below the 65k wrap budget. Cross-shard collisions don't
//   exist because shards' PK spaces don't overlap (each shard owns
//   its own rows entirely).
//
// The card_id portion that previously occupied the high 32 bits is
// gone — every row already carries `card_id` as a column, and the
// PK never needed to encode it for anything beyond uniqueness.

pub fn pack_valid_at(time_ms: u64, sequence: u16) -> u64 {
    (time_ms << 16) | (sequence as u64)
}

pub fn valid_at_time(v: u64) -> u64 {
    v >> 16
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

/// Whether the **stack layout** applies to this `(state, surface)` pair.
/// True when the card is rect-stacked on inventory (state == OnRoot AND
/// surface < 64). False for Free, Slot, OnHex, and world surfaces —
/// those keep the legacy `(q, r)` layout.
///
/// Note: `Slot` rows happen to share the same byte format
/// (`[position:5 | direction:1 | state:2]` with `position = 0`), but
/// the mirror's preserve gate doesn't fire for them — `Slot` cards are
/// always server-authoritative (set by `propose_action`, cleared by
/// `action_completion`). Use [`pack_slot_micro_zone`] for `Slot`
/// writes; share `unpack_stack_micro_zone` for reads since the bit
/// layout is identical.
pub fn is_stack_layout(state: StackedState, surface: u8) -> bool {
    surface < WORLD_LAYER && matches!(state, StackedState::OnRoot)
}

/// Direction bit values within the stack layout. `0 = up / top`,
/// `1 = down / bottom`. Used to disambiguate which side of the chain
/// a card sits on under the single `OnRoot` state.
pub const STACK_DIR_UP: u8 = 0;
pub const STACK_DIR_DOWN: u8 = 1;

/// Pack `(position, direction, state)` under the stack layout:
/// `[position: u5 | direction: u1 | stacked_state: u2]`.
///
/// `position` is the card's index in its chain from the root (1..=31;
/// 0 reserved for "no chain"). Saturates at `0b11111` if higher.
/// `direction` is `0 = up / top` or `1 = down / bottom`. The
/// "server is forcing this position" signal moved out of micro_zone
/// (it used to share bit 2 with the now-`direction` field) and lives
/// in `flags` (`force_position` at bit 11) — see `cards/flags.json`.
///
/// **Only valid for `state == OnRoot` AND `surface < 64`.**
/// Free / ReservedSlot / OnHex / world cards use [`pack_micro_zone`].
pub fn pack_stack_micro_zone(position: u8, direction: u8, state: StackedState) -> u8 {
    debug_assert!(
        matches!(state, StackedState::OnRoot),
        "pack_stack_micro_zone only valid for OnRoot; got {state:?}",
    );
    let pos = position & 0b11111;
    let dir = direction & 0b1;
    (pos << 3) | (dir << 2) | state.to_u2()
}

/// Inverse of [`pack_stack_micro_zone`]. Returns `(position, direction, state)`.
/// The caller is responsible for knowing the byte was packed under the stack
/// layout; reading a legacy-layout byte through here gives nonsense for the
/// `position` and `direction` fields.
pub fn unpack_stack_micro_zone(v: u8) -> (u8, u8, StackedState) {
    let position = (v >> 3) & 0b11111;
    let direction = (v >> 2) & 0b1;
    (position, direction, StackedState::from_u2(v))
}

/// Read just the `position` field under the stack layout.
pub fn micro_zone_position(v: u8) -> u8 {
    (v >> 3) & 0b11111
}

/// Read just the `direction` bit under the stack layout.
/// `0 = up / top`, `1 = down / bottom`.
pub fn micro_zone_direction(v: u8) -> u8 {
    (v >> 2) & 0b1
}

/// Pack a `micro_zone` byte for a `Slot` (parent-pointer mode).
/// Layout matches the stack layout
/// (`[position:5 | direction:1 | state:2]`) but with `position = 0`
/// since position from root is implicit (walk parent pointers via
/// `micro_location`). Direction is stored explicitly so render /
/// parent-resolve don't have to climb the chain to derive it.
pub fn pack_slot_micro_zone(direction: u8) -> u8 {
    let dir = direction & 0b1;
    (dir << 2) | StackedState::Slot.to_u2()
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
        // Time in u48 ms, sequence in u16. `valid_at_time` recovers
        // the time portion; the sequence is opaque (only its
        // uniqueness matters for PK).
        let v = pack_valid_at(0x0000_DEAD_BEEF_1234, 0x5678);
        assert_eq!(valid_at_time(v), 0x0000_DEAD_BEEF_1234);
        assert_eq!(v & 0xFFFF, 0x5678);
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
        let v = pack_micro_zone(5, 3, StackedState::OnHex);
        assert_eq!(unpack_micro_zone(v), (5, 3, StackedState::OnHex));
    }

    #[test]
    fn stack_micro_zone_roundtrip() {
        let v = pack_stack_micro_zone(7, STACK_DIR_UP, StackedState::OnRoot);
        assert_eq!(unpack_stack_micro_zone(v), (7, STACK_DIR_UP, StackedState::OnRoot));
        let v = pack_stack_micro_zone(31, STACK_DIR_DOWN, StackedState::OnRoot);
        assert_eq!(unpack_stack_micro_zone(v), (31, STACK_DIR_DOWN, StackedState::OnRoot));
        // position saturates at 5 bits
        let v = pack_stack_micro_zone(0xFF, STACK_DIR_UP, StackedState::OnRoot);
        assert_eq!(micro_zone_position(v), 31);
        assert_eq!(micro_zone_direction(v), STACK_DIR_UP);
        assert_eq!(micro_zone_state(v), StackedState::OnRoot);
    }

    #[test]
    fn stack_layout_gate() {
        assert!(is_stack_layout(StackedState::OnRoot, 1));
        assert!(!is_stack_layout(StackedState::Free, 1));
        // `Slot` rows are server-authoritative — even though their byte
        // layout matches the stack layout, the mirror's preserve gate
        // doesn't fire for them, so `is_stack_layout` returns false.
        assert!(!is_stack_layout(StackedState::Slot, 1));
        assert!(!is_stack_layout(StackedState::OnHex, 1));
        // World surfaces (>= 64) keep legacy layout regardless of state.
        assert!(!is_stack_layout(StackedState::OnRoot, 64));
        assert!(!is_stack_layout(StackedState::OnRoot, 200));
    }

    #[test]
    fn slot_micro_zone_roundtrip() {
        let v = pack_slot_micro_zone(STACK_DIR_UP);
        assert_eq!(micro_zone_state(v), StackedState::Slot);
        assert_eq!(micro_zone_direction(v), STACK_DIR_UP);
        assert_eq!(micro_zone_position(v), 0);
        let v = pack_slot_micro_zone(STACK_DIR_DOWN);
        assert_eq!(micro_zone_state(v), StackedState::Slot);
        assert_eq!(micro_zone_direction(v), STACK_DIR_DOWN);
        assert_eq!(micro_zone_position(v), 0);
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
