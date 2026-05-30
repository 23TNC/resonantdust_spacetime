// lib.rs
pub mod gate_api;
pub mod gc;
pub mod packed;
pub mod players;
pub mod sequence;

/// The data-shard partition id this auth database owns. Stamped onto
/// the `data_shard` column of rows this module writes (`player_profiles`).
/// `0` today.
///
/// This is the `players` **auth** database: it owns accounts, login, and
/// the identity↔player_id↔(card-shard, soul_id) routing. It does NOT hold
/// cards or souls — those live in the per-shard `cards` databases, and each
/// `Player` row carries the `data_shard` of the card shard it's assigned to.
/// Low-write, so a single auth DB can serve all players; the `cards` shards
/// are what scale out.
pub const DATA_SHARD: u16 = 0;
