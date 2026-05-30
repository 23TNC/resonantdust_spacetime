//! Region-index database — maps a `macro_region` to the `regions` shard
//! (`data_shard`) that holds it. The gateway/client subscribes to this one
//! light index to learn which `regions` shard a region lives on, mirroring how
//! `players` indexes which `cards` shard a player's data lives on.
//!
//! A region's shard assignment is stable, so this is a plain primary-key table
//! (no `valid_at` versioning): [`assign_region`] upserts the current mapping.
//! **Authorization is the gateway's job** — the reducer trusts its arguments,
//! same posture as `cards::spawn_soul` / `regions::acquire_card_shard`.

use spacetimedb::{reducer, table, ReducerContext, Table};

/// This index database's own partition id (single instance today).
pub const DATA_SHARD: u16 = 0;

/// Which `regions` shard holds a given region. One row per region.
#[table(accessor = region_shards, public)]
pub struct RegionShard {
    /// The region key — `macro_region`, bit-identical to `macro_zone`
    /// (`[card_id:u32 | surface:u8 | region_q:i12 | region_r:i12]`).
    #[primary_key]
    pub macro_region: u64,
    /// The `regions` module `DATA_SHARD` id that holds this region.
    pub data_shard: u16,
}

/// Assign (or reassign) `macro_region` to `regions` shard `data_shard`.
/// Upsert — the latest assignment wins.
#[reducer]
pub fn assign_region(
    ctx: &ReducerContext,
    macro_region: u64,
    data_shard: u16,
) -> Result<(), String> {
    // delete-then-insert upsert (delete is a no-op when absent), matching the
    // codebase idiom for single-row keys.
    ctx.db.region_shards().macro_region().delete(macro_region);
    ctx.db.region_shards().insert(RegionShard {
        macro_region,
        data_shard,
    });
    Ok(())
}

/// The `regions` shard holding `macro_region`, if assigned.
pub fn shard_of(ctx: &ReducerContext, macro_region: u64) -> Option<u16> {
    ctx.db
        .region_shards()
        .macro_region()
        .find(macro_region)
        .map(|r| r.data_shard)
}
