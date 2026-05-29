// lib.rs
pub mod cards;
pub mod flags;
pub mod gc;
pub mod packed;
pub mod players;
pub mod sequence;
pub mod souls;
pub mod utilities;

/// The data-shard partition id this module instance owns. Stamped onto every
/// public row this shard writes (the `data_shard` column on `cards` /
/// `players` / `souls`). `0` while a single shard serves everything;
/// horizontal sharding will assign distinct ids per instance. This is the
/// players ("hot") shard — it holds only player / card / soul state; zones and
/// regions live in a separate positionally-referenced database, and recipe
/// validation runs in the gateway.
pub const DATA_SHARD: u16 = 0;
