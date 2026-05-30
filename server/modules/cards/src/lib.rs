// lib.rs
pub mod cards;
pub mod flags;
pub mod gate_api;
pub mod gc;
pub mod packed;
pub mod pending_actions;
pub mod sequence;
pub mod souls;
pub mod utilities;

/// The shard id this `cards` database owns (`0..=`[`packed::CARD_SHARD_MAX`]).
/// `0` while a single shard serves everything; horizontal sharding assigns
/// distinct ids per instance.
///
/// **Folded into every `card_id`, not stored as a column.** `next_card_id`
/// composes each id as `pack_card_id(DATA_SHARD, local)` — the high 12 bits
/// name this shard, the low 20 are a per-shard local counter. So a card's id
/// is self-describing (`packed::card_shard_of(id) == DATA_SHARD`) and there's
/// no separate `data_shard` row column to keep in sync.
///
/// This is the "card" shard — it holds the hot card + soul state for the
/// players assigned to it. Player accounts / login live in the separate
/// `players` auth database, whose `Player.data_shard` tells the client which
/// card shard to subscribe to. Recipe validation runs in the gateway.
pub const DATA_SHARD: u16 = 0;
