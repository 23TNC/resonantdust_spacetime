//! Gate-facing write reducers for this `regions` shard. Authorization is the
//! gateway's job — these trust their arguments.
//!
//! The card-shard ref-count reducers (`acquire_card_shard` /
//! `release_card_shard`) live in `card_shards`; this adds the recipe tile
//! write. (`request_zone` / terrain generation already exist in `regions`.)

use spacetimedb::{reducer, ReducerContext};

use crate::zones;

/// Set a single tile of `zone_id` at `(row, col)` to `def_id` with per-row
/// stocks, stamped at `time_ms`. The gateway resolves `zone_id` from its
/// gathered zone snapshot. A recipe tile effect (`<tile>.aspect.X.set`).
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn set_tile(
    ctx: &ReducerContext,
    zone_id: u32,
    time_ms: u64,
    row: u8,
    col: u8,
    def_id: u16,
    stock0: u8,
    stock1: u8,
) -> Result<(), String> {
    if zones::set_tile_at(ctx, zone_id, time_ms, row, col, def_id, stock0, stock1).is_none() {
        return Err(format!(
            "set_tile: zone {zone_id} not found or row/col out of range"
        ));
    }
    Ok(())
}
