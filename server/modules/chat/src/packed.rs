//! Bit-packing helpers used by the chat module.
//!
//! Chat consumes only the `valid_at` PK packers — every chat row
//! follows the same `(time_ms_u48 << 16) | sequence_u16` shape the
//! shard uses, but chat has no card / zone / definition / recipe
//! state, so the rest of `content/src/packed.rs` is irrelevant
//! here.
//!
//! Keeping these two helpers inlined avoids a `resonantdust-content`
//! path-dep + bind-mount on chat just to share six lines of
//! bit-shifting. If chat ever grows card-bearing tables, switch to
//! `pub use resonantdust_content::packed::*;` and re-add the dep —
//! the shapes match by construction since the constants live in
//! `content/src/packed.rs` (and shard's mirror).

/// Pack `(time_ms, sequence)` into the u64 PK. Mirrors
/// `resonantdust_content::packed::pack_valid_at`.
pub fn pack_valid_at(time_ms: u64, sequence: u16) -> u64 {
    (time_ms << 16) | (sequence as u64)
}

/// Extract the `time_ms` half of a packed `valid_at`. Mirrors
/// `resonantdust_content::packed::valid_at_time`.
pub fn valid_at_time(v: u64) -> u64 {
    v >> 16
}
