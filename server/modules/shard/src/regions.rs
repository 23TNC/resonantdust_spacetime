//! Regions — coarse spawn-gating over the zone grid.
//!
//! A `Region` governs a `REGION_SIZE × REGION_SIZE` (8×8 = 64) block of zones.
//! It carries two 64-bit fields over its constituent zones: `zone_presence`
//! (this zone *may* be spawned) and `zone_available` (this zone *has* been
//! spawned). The `request_zone` reducer is the on-demand spawn gateway: it
//! spawns a zone only if its region permits it and it isn't already spawned.
//!
//! This replaces the old eager radius-2 disk seed (`utilities::bootstrap` →
//! `world_gen::generate_forest_terrain`). The world starts empty-but-spawnable:
//! `bootstrap` seeds the origin region with full presence, and zones come into
//! existence as they're requested.

use spacetimedb::{reducer, table, ReducerContext, Table};

use crate::packed::{
    owner_of, pack_macro_region, pack_valid_at, pack_zone_definition, region_of_zone, surface_of,
    unpack_macro_zone, valid_at_time, INVENTORY_LAYER, WORLD_LAYER,
};
use crate::sequence;
use crate::world_gen;
use crate::zones;

#[table(accessor = regions, public)]
pub struct Region {
    #[primary_key]
    pub valid_at: u64,
    /// Data-shard partition this row belongs to (`crate::DATA_SHARD`; `0` today).
    pub data_shard: u16,
    /// Packed region key — same layout as `macro_zone`
    /// (`[card_id:u32 | surface:u8 | region_q:i12 | region_r:i12]`) but in
    /// region units (1 region = 8 zones). Immutable identity (regions never
    /// move), so there's no separate `region_id`; history versions share it.
    #[index(btree)]
    pub macro_region: u64,
    /// Bit `i` set = the zone at this region's slot `i` MAY be spawned. Slot
    /// `i` ↔ zone `(region_q*8 + i%8, region_r*8 + i/8)` (see `region_of_zone`).
    pub zone_presence: u64,
    /// Bit `i` set = the zone at slot `i` HAS been spawned (a `zones` row
    /// exists). Set by `request_zone` after a successful spawn.
    pub zone_available: u64,
}

fn now_ms(ctx: &ReducerContext) -> u64 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64
}

/// Latest version row for `macro_region` (largest `valid_at` time), or `None`.
pub fn latest_for(ctx: &ReducerContext, macro_region: u64) -> Option<Region> {
    ctx.db
        .regions()
        .macro_region()
        .filter(macro_region)
        .max_by_key(|r| valid_at_time(r.valid_at))
}

/// Write a region version row stamped at `time_ms`. Mirrors `zones::write_at`:
/// "last write at this (macro_region, time_ms) wins" — same-ms rows for this
/// region are deleted first so they don't accumulate under the sequence PK.
fn write_at(ctx: &ReducerContext, mut region: Region, time_ms: u64) -> Region {
    let macro_region = region.macro_region;
    let stale: Vec<u64> = ctx
        .db
        .regions()
        .macro_region()
        .filter(macro_region)
        .filter(|r| valid_at_time(r.valid_at) == time_ms)
        .map(|r| r.valid_at)
        .collect();
    for v in stale {
        ctx.db.regions().valid_at().delete(v);
    }
    region.valid_at = pack_valid_at(time_ms, sequence::next_sequence(ctx));
    ctx.db.regions().insert(region)
}

/// Seed the world's origin region — `(0, 0)` on `WORLD_LAYER`, owner `0` — with
/// every zone spawnable (`zone_presence = u64::MAX`) and none yet spawned
/// (`zone_available = 0`). Idempotent: no-op if the row already exists.
pub fn seed_world_region(ctx: &ReducerContext) {
    let macro_region = pack_macro_region(0, WORLD_LAYER, 0, 0);
    if latest_for(ctx, macro_region).is_some() {
        return;
    }
    write_at(
        ctx,
        Region {
            valid_at: 0,
            data_shard: crate::DATA_SHARD,
            macro_region,
            zone_presence: u64::MAX,
            zone_available: 0,
        },
        now_ms(ctx),
    );
}

/// Seed a freshly-spawned soul's inventory region: only the `(0, 0)` zone is
/// present (bit 0), nothing yet available. The inventory Zone itself is no
/// longer eagerly created — the client's region gate requests it on demand when
/// the inventory viewport opens, the same path world zones take. Idempotent.
pub fn seed_soul_inventory_region(ctx: &ReducerContext, soul_card_id: u32, time_ms: u64) {
    let macro_region = pack_macro_region(soul_card_id, INVENTORY_LAYER, 0, 0);
    if latest_for(ctx, macro_region).is_some() {
        return;
    }
    write_at(
        ctx,
        Region {
            valid_at: 0,
            data_shard: crate::DATA_SHARD,
            macro_region,
            zone_presence: 1, // bit 0 → the (0, 0) inventory zone
            zone_available: 0,
        },
        time_ms,
    );
}

/// Set `zone_available` bit `bit` on the latest version of `macro_region`,
/// writing a new version row stamped at `time_ms`. No-op if the region is gone
/// or the bit is already set.
fn set_available_bit(ctx: &ReducerContext, macro_region: u64, bit: u8, time_ms: u64) {
    let Some(mut region) = latest_for(ctx, macro_region) else {
        return;
    };
    let mask = 1u64 << bit;
    if region.zone_available & mask != 0 {
        return;
    }
    region.zone_available |= mask;
    write_at(ctx, region, time_ms);
}

/// Request that the zone at `macro_zone` be spawned. Region-gated and
/// idempotent:
/// - No governing region, or the region's `zone_presence` bit is clear → no-op.
/// - Already spawned (available bit set AND a `zones` row exists) → no-op.
/// - Otherwise spawn the zone — biome generation on the world surface, an empty
///   rect grid on any other surface — and set the region's `zone_available` bit.
///
/// The spawn is guarded on the zone row actually being absent, so an
/// inconsistent "bit clear but row exists" state just reconciles the bit rather
/// than creating a duplicate zone.
///
/// New rows are stamped at the **client-supplied** time (`effective_now_ms`,
/// the `min(client, server)` clamp recipes use), NOT `now()`. The client
/// renders on a buffered clock ~`clientDelay` (1.5–5s) behind true server time;
/// stamping at server-now would land the zone's `valid_at` in the client's
/// buffered future, so `ValidAtTable.promote` wouldn't surface it (the zone
/// wouldn't appear) until the lag elapsed. Stamping at the client's time lands
/// it on the client's timeline so it shows as soon as it round-trips back.
#[reducer]
pub fn request_zone(
    ctx: &ReducerContext,
    client_time_ms: u64,
    macro_zone: u64,
) -> Result<(), String> {
    let (macro_region, bit) = region_of_zone(macro_zone);
    let mask = 1u64 << bit;

    let Some(region) = latest_for(ctx, macro_region) else {
        return Ok(()); // no region governs this zone
    };

    let row_exists = zones::latest_for(ctx, macro_zone).is_some();

    // Already available: bit set AND a row exists.
    if region.zone_available & mask != 0 && row_exists {
        return Ok(());
    }

    // Presence must permit spawning.
    if region.zone_presence & mask == 0 {
        return Ok(());
    }

    // About to write — resolve the client-aligned timestamp (rejects on
    // excessive drift) and stamp every new row in this reducer from it.
    let now = crate::cards::effective_now_ms(ctx, client_time_ms)?;

    if !row_exists {
        let surface = surface_of(macro_zone);
        if surface == WORLD_LAYER {
            let (zq, zr) = unpack_macro_zone(macro_zone);
            let tiles = world_gen::generate_zone_tiles(zq, zr, world_gen::WORLD_SEED);
            let zone_id = zones::next_zone_id(ctx);
            zones::create_at(
                ctx,
                zone_id,
                macro_zone,
                pack_zone_definition(world_gen::TILE_ZONE_TYPE),
                /* owner_id */ 0,
                tiles,
                now,
            );
        } else {
            // Forward-looking: no non-world region is seeded yet, so this branch
            // is currently unreachable.
            zones::create_rect_at(ctx, macro_zone, owner_of(macro_zone), now)?;
        }
    }

    set_available_bit(ctx, macro_region, bit, now);
    Ok(())
}
