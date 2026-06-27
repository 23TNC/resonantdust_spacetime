// lib.rs
//
// Unified data-shard module. One binary, published to BOTH the owner-card DBs
// and the region DBs (see `cards::ShardIdentity` for how a single binary stamps
// `card_id`s with the right database bit). The owner-card surface
// (`cards`/`souls`/holds) and the region surface (`zones`/`regions`/tile-cards)
// share one canonical `Card` definition and one set of bitemporal write
// primitives — the former `regions` module's drifted partial copy is gone.
pub mod card_shards;
pub mod cards;
pub mod flags;
pub mod gate_api;
pub mod gc;
pub mod movement;
pub mod oplog;
pub mod packed;
pub mod pending_actions;
pub mod place;
pub mod regions;
pub mod sequence;
pub mod souls;
pub mod tiles;
pub mod utilities;
pub mod zones;

/// Default shard id for this deployment (`0..=`[`packed::CARD_SHARD_MAX`]) when
/// its [`cards::ShardIdentity`] is unseeded. `0` while a single shard serves
/// everything; horizontal sharding assigns distinct ids per instance.
///
/// **Folded into every `card_id`, not stored as a column.** `next_card_id`
/// composes each id as `pack_card_id(card_db, shard, local)` — the top bit names
/// the database family (cards vs region, from `ShardIdentity`), the shard band
/// names this instance, the low 20 are a per-shard local counter. So a card's id
/// is self-describing and the gateway routes by it with no `data_shard` column.
///
/// This `shard` module is the unified data shard: deployed to a cards DB it
/// holds the hot card + soul state for the players assigned to it; deployed to a
/// region DB it holds zones/regions/tile-cards. Player accounts / login live in
/// the separate `players` auth database, whose `Player.data_shard` tells the
/// client which cards shard to subscribe to. Recipe validation runs in the gateway.
pub const DATA_SHARD: u16 = 0;
