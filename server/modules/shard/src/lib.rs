// lib.rs
pub mod action_completion;
pub mod actions;
pub mod blueprints;
pub mod cards;
pub mod flags;
pub mod gc;
pub mod lifecycle_pending;
pub mod mini_zone;
pub mod movement;
pub mod packed;
pub mod pending_actions;
pub mod place;
pub mod players;
pub mod recipe_eval;
pub mod regions;
pub mod sequence;
pub mod souls;
pub mod utilities;
pub mod world_gen;
pub mod zones;

/// The data-shard partition id this module instance owns. Stamped onto every
/// public row this shard writes (the `data_shard` column on `cards` / `players`
/// / `souls` / `zones` / `regions` / …). `0` while a single shard serves
/// everything; horizontal sharding will assign distinct ids per instance.
pub const DATA_SHARD: u16 = 0;