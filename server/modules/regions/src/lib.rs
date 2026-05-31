// lib.rs
pub mod card_shards;
pub mod cards;
pub mod gate_api;
pub mod gc;
pub mod packed;
pub mod regions;
pub mod sequence;
pub mod time;
pub mod world_gen;
pub mod zones;

// No `DATA_SHARD` constant: `regions`/`zones` rows no longer carry a
// data-shard stamp. Regions are positionally sharded (a separate axis from
// the owner-sharded `cards`); when that lands, a planned light region-index
// database will map region/position → which `regions` shard holds it
// — mirroring how `players` indexes which `cards` shard a player's data
// lives on. The `card_shards` table here still keys on the *cards* shard id,
// which is folded into each `card_id` (see the `cards` module).
