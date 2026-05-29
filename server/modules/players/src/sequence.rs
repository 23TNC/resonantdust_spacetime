//! Global monotonic u16 sequence allocator for `valid_at` PK
//! disambiguation.
//!
//! The cards / zones / souls / players tables share a packed-u64
//! primary key layout: `(time_ms_u48 << 16) | seq_u16`. The time
//! portion is u48 milliseconds since Unix epoch (8920 years of
//! runway); the low 16 bits are a sequence number that disambiguates
//! same-millisecond writes within a single SpacetimeDB module.
//!
//! `next_sequence` returns a fresh u16 on every call. Read-modify-
//! write against a single-row counter table, same pattern as
//! `cards::next_card_id`. Wraps at 65536 via `wrapping_add(1)` — a
//! wrap only collides if 65k writes happened between two writes to
//! the same millisecond, which would require ~65k transactions all
//! stamping rows at the same future ms, far beyond realistic
//! intra-module write rates. If/when shards split, each shard's
//! module has its own independent counter; cross-shard collisions
//! aren't possible because each shard's PKs aren't compared with
//! another's.

use spacetimedb::{table, ReducerContext, Table};

/// Single-row counter table holding the next sequence value to hand
/// out. PK is always `0` — one-row pattern matching
/// `cards::CardIdCounter`.
#[table(accessor = sequence_counter)]
pub struct SequenceCounter {
    #[primary_key]
    pub id: u8,
    pub next: u16,
}

/// Allocate the next u16 sequence number. Wraps at 65536.
///
/// Implementation mirrors `cards::next_card_id`: read the row, return
/// its current value, write back `value + 1` (wrapping). Lazy seed on
/// first call after a fresh deployment — starts at 0.
pub fn next_sequence(ctx: &ReducerContext) -> u16 {
    if let Some(counter) = ctx.db.sequence_counter().id().find(0) {
        let allocated = counter.next;
        ctx.db.sequence_counter().id().delete(0);
        ctx.db.sequence_counter().insert(SequenceCounter {
            id: 0,
            next: allocated.wrapping_add(1),
        });
        allocated
    } else {
        ctx.db.sequence_counter().insert(SequenceCounter {
            id: 0,
            next: 1,
        });
        0
    }
}
