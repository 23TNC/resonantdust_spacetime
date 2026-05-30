//! Server-side bit masks for the `cards.flags_state` and
//! `cards.flags_bk` host integers, loaded once from the content
//! crate's `cards/flags.json` registry and cached for the life of
//! the module.
//!
//! Replaces the per-module `const FLAG_*` declarations that used to
//! be scattered across `cards.rs` / `actions.rs` / `utilities.rs` /
//! etc., each independently hand-encoding the same bit positions.
//! The single source of truth is `content/cards/flags.json`; this
//! module asks the content crate at first access and caches the
//! results in `OnceLock`-backed structs.
//!
//! # Unified hold counts
//!
//! Under the unified-hold-counts rework (see
//! `docs/UNIFIED_HOLD_COUNTS.md`) all hold-style state lives in
//! `flags_bk` as refcount fields with the 'lock = never-released
//! +1' idiom. The former single-bit `slot_hold` / `position_locked` /
//! `drop_hold` / `drop_locked` entries in `flags_state` are gone;
//! their semantics are subsumed by `slot_hold_count` /
//! `position_hold_count` / `drop_hold_count` here.
//!
//! # Usage
//!
//! ```rust,no_run
//! // Test a state flag bit:
//! if card.flags_state & state_flags().dead != 0 { ... }
//!
//! // Test a hold count (was a single-bit check pre-rework):
//! if card.flags_bk & bk_flags().slot_hold_count_mask != 0 { ... }
//!
//! // Read a multi-bit field:
//! let count = (card.flags_bk & bk_flags().slot_share_count_mask)
//!     >> bk_flags().slot_share_count_shift;
//! ```
//!
//! # Failure mode
//!
//! A missing flag entry panics at first access. These structs encode
//! every flag the server reducer code depends on — if a flag is
//! removed from `flags.json`, the panic is the failure signal (catch
//! at build / test time, not in production traffic).

use std::sync::OnceLock;

use resonantdust_content::flags_core::{flag_bit, flag_field};

/// Bit masks for every entry in `cards_state`. Each `u32` is the
/// pre-built mask (already `1 << bit` for single bits, or
/// `((1 << width) - 1) << shift` for multi-bit fields). The former
/// hold-style entries (`slot_hold`, `position_locked`, `drop_hold`,
/// `drop_locked`) are intentionally absent — their semantics moved to
/// `flags_bk` counts under the unified-hold-counts rework.
pub struct StateFlags {
    pub dead: u32,
    /// Server *requires* the row's position exactly. Mirror-side
    /// splice winners-take-the-slot; cards above re-anchor over the
    /// new arrival. See `docs/POS_NEED_WANT.md` (forthcoming) and
    /// the flag description in `content/cards/flags.json`.
    pub pos_need: u32,
    pub magnetic: u32,
    pub surface_locked: u32,
    pub is_owned_by_player: u32,

    /// `progress_style` field — shift + mask form, since callers
    /// need to both read (`(host & mask) >> shift`) and write
    /// (`(host & !mask) | ((value << shift) & mask)`).
    pub progress_style_mask: u32,
    pub progress_style_shift: u32,

    pub portrait_id_mask: u32,
    pub portrait_id_shift: u32,

    /// Server *prefers* the row's position. Mirror-side splice
    /// stacks-on-conflict (existing card keeps the slot, new card
    /// stacks above). Sibling of `pos_need`.
    pub pos_want: u32,

    /// Card was generated from zone tile data (a materialized tile-card).
    /// Static — set once at materialize, never flipped — so it is safe in
    /// `flags_state` where the bit-diff propagator carries it forward.
    pub zone_born: u32,
}

/// Bit masks for every entry in `cards_bk`. Holds the unified
/// count fields (position_hold, slot_share, drop_hold, slot_hold)
/// plus the concurrency caps (touch_count, server_count) plus the
/// existing dirty / preserve markers.
pub struct BkFlags {
    pub position_dirty: u32,
    pub position_preserve: u32,
    pub data_dirty: u32,
    pub data_preserve: u32,

    pub position_hold_count_mask: u32,
    pub position_hold_count_shift: u32,
    pub position_hold_count_max: u32,

    pub slot_share_count_mask: u32,
    pub slot_share_count_shift: u32,
    pub slot_share_count_max: u32,

    pub drop_hold_count_mask: u32,
    pub drop_hold_count_shift: u32,
    pub drop_hold_count_max: u32,

    pub slot_hold_count_mask: u32,
    pub slot_hold_count_shift: u32,
    pub slot_hold_count_max: u32,

    pub touch_count_mask: u32,
    pub touch_count_shift: u32,
    pub touch_count_max: u32,

    pub server_count_mask: u32,
    pub server_count_shift: u32,
    pub server_count_max: u32,

    pub tile_stock_0_mask: u32,
    pub tile_stock_0_shift: u32,
    pub tile_stock_0_max: u32,

    pub tile_stock_1_mask: u32,
    pub tile_stock_1_shift: u32,
    pub tile_stock_1_max: u32,

    /// Discriminates `micro_location` (set → root card_id; clear → loose
    /// coords). In `flags_bk` so it stays lockstep with `micro_location`.
    pub micro_is_card: u32,

    /// `stack_state` — 2-bit branch/kind gated on `micro_is_card`.
    pub stack_state_mask: u32,
    pub stack_state_shift: u32,
    pub stack_state_max: u32,

    /// `stack_index` — 4-bit slot index within a branch (0..15).
    pub stack_index_mask: u32,
    pub stack_index_shift: u32,
    pub stack_index_max: u32,
}

/// Client-side concurrency cap on `touch_count`. A `propose_action`
/// targeting a card whose `touch_count >= TOUCH_COUNT_CLIENT_CAP` is
/// rejected by `validate_bindings`. Cap = 3 so the underlying hold
/// counts (slot_share, position_hold, etc., all u3 saturating at 7)
/// have headroom before saturation under realistic gameplay.
pub const TOUCH_COUNT_CLIENT_CAP: u32 = 3;

/// Server-side concurrency cap on `server_count`. Same idea as
/// `TOUCH_COUNT_CLIENT_CAP`, just on the server-internal field.
pub const SERVER_COUNT_CAP: u32 = 3;

/// Lazy accessor for the cached `cards_state` mask struct.
pub fn state_flags() -> &'static StateFlags {
    static CACHE: OnceLock<StateFlags> = OnceLock::new();
    CACHE.get_or_init(|| {
        let dead = bit_mask("cards_state", "dead");
        let pos_need = bit_mask("cards_state", "pos_need");
        let magnetic = bit_mask("cards_state", "magnetic");
        let surface_locked = bit_mask("cards_state", "surface_locked");
        let is_owned_by_player = bit_mask("cards_state", "is_owned_by_player");
        let (progress_style_mask, progress_style_shift, _) =
            field_parts("cards_state", "progress_style");
        let (portrait_id_mask, portrait_id_shift, _) =
            field_parts("cards_state", "portrait_id");
        let pos_want = bit_mask("cards_state", "pos_want");
        let zone_born = bit_mask("cards_state", "zone_born");

        StateFlags {
            dead,
            pos_need,
            magnetic,
            surface_locked,
            is_owned_by_player,
            progress_style_mask,
            progress_style_shift,
            portrait_id_mask,
            portrait_id_shift,
            pos_want,
            zone_born,
        }
    })
}

/// Lazy accessor for the cached `cards_bk` mask struct.
pub fn bk_flags() -> &'static BkFlags {
    static CACHE: OnceLock<BkFlags> = OnceLock::new();
    CACHE.get_or_init(|| {
        let position_dirty = bit_mask("cards_bk", "position_dirty");
        let position_preserve = bit_mask("cards_bk", "position_preserve");
        let data_dirty = bit_mask("cards_bk", "data_dirty");
        let data_preserve = bit_mask("cards_bk", "data_preserve");
        let (position_hold_count_mask, position_hold_count_shift, position_hold_count_max) =
            field_parts("cards_bk", "position_hold_count");
        let (slot_share_count_mask, slot_share_count_shift, slot_share_count_max) =
            field_parts("cards_bk", "slot_share_count");
        let (drop_hold_count_mask, drop_hold_count_shift, drop_hold_count_max) =
            field_parts("cards_bk", "drop_hold_count");
        let (slot_hold_count_mask, slot_hold_count_shift, slot_hold_count_max) =
            field_parts("cards_bk", "slot_hold_count");
        let (touch_count_mask, touch_count_shift, touch_count_max) =
            field_parts("cards_bk", "touch_count");
        let (server_count_mask, server_count_shift, server_count_max) =
            field_parts("cards_bk", "server_count");
        let (tile_stock_0_mask, tile_stock_0_shift, tile_stock_0_max) =
            field_parts("cards_bk", "tile_stock_0");
        let (tile_stock_1_mask, tile_stock_1_shift, tile_stock_1_max) =
            field_parts("cards_bk", "tile_stock_1");
        let micro_is_card = bit_mask("cards_bk", "micro_is_card");
        let (stack_state_mask, stack_state_shift, stack_state_max) =
            field_parts("cards_bk", "stack_state");
        let (stack_index_mask, stack_index_shift, stack_index_max) =
            field_parts("cards_bk", "stack_index");

        BkFlags {
            position_dirty,
            position_preserve,
            data_dirty,
            data_preserve,
            position_hold_count_mask,
            position_hold_count_shift,
            position_hold_count_max,
            slot_share_count_mask,
            slot_share_count_shift,
            slot_share_count_max,
            drop_hold_count_mask,
            drop_hold_count_shift,
            drop_hold_count_max,
            slot_hold_count_mask,
            slot_hold_count_shift,
            slot_hold_count_max,
            touch_count_mask,
            touch_count_shift,
            touch_count_max,
            server_count_mask,
            server_count_shift,
            server_count_max,
            tile_stock_0_mask,
            tile_stock_0_shift,
            tile_stock_0_max,
            tile_stock_1_mask,
            tile_stock_1_shift,
            tile_stock_1_max,
            micro_is_card,
            stack_state_mask,
            stack_state_shift,
            stack_state_max,
            stack_index_mask,
            stack_index_shift,
            stack_index_max,
        }
    })
}

// ---------- internals ----------

/// Look up a single-bit flag and return its pre-shifted mask.
/// Panics if the registry says the flag isn't declared — the structs
/// above are the authoritative list of every flag the server depends
/// on, so a missing entry is a content-crate / server-code mismatch
/// that should fail loudly.
fn bit_mask(field: &str, name: &str) -> u32 {
    let bit = flag_bit(field, name)
        .unwrap_or_else(|e| panic!("flags: registry build failed: {e}"))
        .unwrap_or_else(|| panic!("flags: {field}.{name} not declared as single-bit"));
    1u32 << bit
}

/// Look up a multi-bit field and return `(mask, shift, max_value)`.
/// `mask` is the pre-built window mask; `shift` is the low bit
/// position; `max_value` is `(1 << width) - 1` (handy for saturating
/// arithmetic on refcount fields).
fn field_parts(field: &str, name: &str) -> (u32, u32, u32) {
    let f = flag_field(field, name)
        .unwrap_or_else(|e| panic!("flags: registry build failed: {e}"))
        .unwrap_or_else(|| panic!("flags: {field}.{name} not declared as multi-bit field"));
    let value_mask: u32 = (((1u64 << f.width) - 1) & 0xFFFF_FFFF) as u32;
    let mask: u32 = value_mask << f.shift;
    (mask, f.shift as u32, value_mask)
}
