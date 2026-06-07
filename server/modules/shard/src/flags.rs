//! Server-side bit masks for the cards table flag columns, loaded once from the
//! shared [`resonantdust_codec::flags`] registry and cached for the life of the
//! module.
//!
//! Two columns are surfaced here:
//!
//!   - [`state_flags`] — the single-bit gameplay state bits within the
//!     propagating `flags` word (`dead`, `pos_need`, `pos_want`, `player_owned`,
//!     `surface_locked`, `zone_born`). The multi-bit fields in `flags`
//!     (`stack`/`index`, the refcount holds) are reached through
//!     [`resonantdust_codec::card_model`] instead — it owns that arithmetic.
//!   - [`bk_flags`] — the dirty / preserve markers within the non-propagating
//!     `flags_bk` byte (server-managed by [`crate::cards::write_at`]).
//!
//! A missing flag entry panics at first access (build/test-time failure signal).

use std::sync::OnceLock;

use resonantdust_codec::flags::flag_bit;

/// Single-bit state masks within the propagating `flags` word.
pub struct StateFlags {
    pub dead: u32,
    /// Server *requires* the row's position exactly (mirror splice
    /// winners-take-the-slot). Blocks demotion.
    pub pos_need: u32,
    /// Server *prefers* the row's position (mirror splice stacks-on-conflict).
    pub pos_want: u32,
    /// The owner chain names a player here (`owner_id` is a `player_id`).
    pub player_owned: u32,
    pub surface_locked: u32,
    /// Card was materialized from zone tile data (a tile-card).
    pub zone_born: u32,
}

/// Dirty / preserve markers within the non-propagating `flags_bk` byte.
pub struct BkFlags {
    pub position_dirty: u8,
    pub position_preserve: u8,
    pub data_dirty: u8,
    pub data_preserve: u8,
}

/// Client-side concurrency cap on `touch_count`. A `propose_action` targeting a
/// card whose `touch_count >= TOUCH_COUNT_CLIENT_CAP` is rejected.
pub const TOUCH_COUNT_CLIENT_CAP: u32 = 3;

/// Server-side concurrency cap on `server_count`.
pub const SERVER_COUNT_CAP: u32 = 3;

/// Lazy accessor for the cached state-bit masks.
pub fn state_flags() -> &'static StateFlags {
    static CACHE: OnceLock<StateFlags> = OnceLock::new();
    CACHE.get_or_init(|| StateFlags {
        dead: bit_mask("flags", "dead"),
        pos_need: bit_mask("flags", "pos_need"),
        pos_want: bit_mask("flags", "pos_want"),
        player_owned: bit_mask("flags", "player_owned"),
        surface_locked: bit_mask("flags", "surface_locked"),
        zone_born: bit_mask("flags", "zone_born"),
    })
}

/// Lazy accessor for the cached dirty/preserve markers.
pub fn bk_flags() -> &'static BkFlags {
    static CACHE: OnceLock<BkFlags> = OnceLock::new();
    CACHE.get_or_init(|| BkFlags {
        position_dirty: bit_mask8("flags_bk", "position_dirty"),
        position_preserve: bit_mask8("flags_bk", "position_preserve"),
        data_dirty: bit_mask8("flags_bk", "data_dirty"),
        data_preserve: bit_mask8("flags_bk", "data_preserve"),
    })
}

// ---------- internals ----------

fn bit_mask(field: &str, name: &str) -> u32 {
    let bit = flag_bit(field, name)
        .unwrap_or_else(|| panic!("flags: {field}.{name} not declared as single-bit"));
    1u32 << bit
}

fn bit_mask8(field: &str, name: &str) -> u8 {
    let bit = flag_bit(field, name)
        .unwrap_or_else(|| panic!("flags: {field}.{name} not declared as single-bit"));
    1u8 << bit
}
