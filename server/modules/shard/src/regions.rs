//! Regions — coarse spawn-gating over the zone grid.
//!
//! A `Region` governs a `REGION_SIZE × REGION_SIZE` (7×7 = 49) block of zones.
//! It carries two 64-bit fields over its constituent zones: `zone_presence`
//! (this zone *may* be spawned) and `zone_available` (this zone *has* been
//! spawned). The `request_zone` reducer is the on-demand spawn gateway: it
//! spawns a zone only if its region permits it and it isn't already spawned.
//!
//! **Current-value, not version-history.** Unlike `zones`/`cards` (which version
//! on the client's buffered clock so rows appear on the client timeline), a region
//! is a single mutable row keyed by `macro_region`. `zone_available` is a plain
//! monotonic bit-accumulator: set bit `i` when zone `i`'s row is created, clear it
//! if that zone is ever removed; to stop a zone regenerating, clear its
//! `zone_presence` bit. No `valid_at`, no forward-propagation, no GC. A zone whose
//! row is future-stamped can read `available` before it "exists" on the client
//! clock — that's fine: the client just won't re-request a zone that's already
//! coming.
//!
//! This replaces the old eager radius-2 disk seed (`utilities::bootstrap` →
//! `world_gen::generate_forest_terrain`). The world starts empty-but-spawnable:
//! `bootstrap` seeds the origin region with full presence, and zones come into
//! existence as they're requested.

use spacetimedb::{reducer, table, ReducerContext, Table};

use crate::packed::{
    owner_of, pack_macro_region, pack_zone_definition, region_of_zone, surface_of, WORLD_LAYER,
};
use crate::zones;

/// Tile card_type (`content/cards/types.json` → `tile: 7`) — the Zone row's
/// `packed_definition` type. Worldgen itself moved to the gate (plan
/// `01_gate_authority_pivot`); the gate supplies the tile bytes.
const TILE_ZONE_TYPE: u8 = 7;

#[table(accessor = regions, public)]
pub struct Region {
    /// Packed region key — same layout as `macro_zone`
    /// (`[card_id:u32 | surface:u8 | region_q:i12 | region_r:i12]`) but in
    /// region units (1 region = 7 zones). Immutable identity (regions never
    /// move) AND the primary key: one current-value row per region.
    #[primary_key]
    pub macro_region: u64,
    /// Bit `i` set = the zone at this region's slot `i` MAY be spawned. Slot
    /// `i` ↔ zone `(region_q*8 + i%8, region_r*8 + i/8)` (see `region_of_zone`).
    pub zone_presence: u64,
    /// Bit `i` set = the zone at slot `i` HAS been spawned (a `zones` row
    /// exists). Set by `request_zone` after a successful spawn.
    pub zone_available: u64,
    /// Disk radius (in tiles) of this region's owner-origin. The single bound
    /// for the whole region: `zone_presence` carries exactly the zones whose
    /// tiles fall within `distance` of the origin tile `(0,0)`, and
    /// `request_zone` masks each spawned zone's tiles to that same disk. World
    /// regions use `u16::MAX` (effectively unbounded); a container uses
    /// `inventory − 1` (gate-supplied at `ensure_region`).
    pub distance: u16,
}

/// Cube/hex distance between two axial tile coords:
/// `(|dq| + |dr| + |dq+dr|) / 2`. Mirrors `zones::in_disk`.
fn hex_dist(aq: i32, ar: i32, bq: i32, br: i32) -> i32 {
    let dq = aq - bq;
    let dr = ar - br;
    (dq.abs() + dr.abs() + (dq + dr).abs()) / 2
}

/// `zone_presence` for a region: bit set for every one of the region's 49 zone
/// slots that has at least one tile within `distance` of the owner-origin tile
/// `(0,0)`. Pure geometry off `distance` — the same disk `request_zone` masks
/// tiles to. `u16::MAX` distance lights every bit (world behaviour).
fn presence_for_disk(macro_region: u64, distance: u16) -> u64 {
    use crate::packed::{world_tile, REGION_SIZE, ZONE_SIZE};
    let (region_q, region_r) = crate::packed::unpack_macro_zone(macro_region);
    let d = distance as i32;
    let mut bits = 0u64;
    let slots = (REGION_SIZE * REGION_SIZE) as u8; // 7×7 = 49
    for bit in 0u8..slots {
        let (zq, zr) = crate::packed::zone_of_region_slot(region_q, region_r, bit);
        'cells: for lr in 0..ZONE_SIZE {
            for lc in 0..ZONE_SIZE {
                if hex_dist(world_tile(zq, lc as u8), world_tile(zr, lr as u8), 0, 0) <= d {
                    bits |= 1u64 << bit;
                    break 'cells;
                }
            }
        }
    }
    bits
}

/// The current row for `macro_region`, or `None` if no region governs it yet.
pub fn get_region(ctx: &ReducerContext, macro_region: u64) -> Option<Region> {
    ctx.db.regions().macro_region().find(macro_region)
}

/// Seed the world's origin region — `(0, 0)` on `WORLD_LAYER`, owner `0` — with
/// every zone spawnable (`zone_presence = u64::MAX`) and none yet spawned
/// (`zone_available = 0`). Idempotent: no-op if the row already exists.
pub fn seed_world_region(ctx: &ReducerContext) {
    let macro_region = pack_macro_region(0, WORLD_LAYER, 0, 0);
    if get_region(ctx, macro_region).is_some() {
        return;
    }
    ctx.db.regions().insert(Region {
        macro_region,
        zone_presence: u64::MAX,
        zone_available: 0,
        distance: u16::MAX,
    });
}

/// Ensure a region governs `macro_zone`, creating it on first need. No-op if a
/// region already exists for the derived `macro_region`. Otherwise insert one with
/// surface-keyed presence (`zone_available = 0` — nothing spawned yet); the
/// client's region gate then requests the zones it wants. This is the
/// client-driven path that replaces the old in-`spawn_soul` inventory-region seed
/// (now a cross-DB call the gate relays).
///
/// Presence is the disk of radius `distance` (tiles) around the owner-origin tile
/// `(0,0)` — see [`presence_for_disk`]. Uniform across surfaces: the gate supplies
/// `distance` (`inventory − 1` for a container, `u16::MAX` for the world, which
/// lights every bit). `client_time_ms` is unused now that the region is
/// current-value, but its NAME is part of the reducer's arg schema — the gate
/// sends args by name, so renaming it (even to `_client_time_ms`) breaks the call
/// with "unknown reducer argument". It stays `client_time_ms`.
#[reducer]
pub fn ensure_region(
    ctx: &ReducerContext,
    client_time_ms: u64,
    macro_zone: u64,
    distance: u16,
) -> Result<(), String> {
    let _ = client_time_ms; // unused: region is current-value, not timeline-stamped
    let (macro_region, _bit) = region_of_zone(macro_zone);
    if get_region(ctx, macro_region).is_some() {
        return Ok(());
    }
    ctx.db.regions().insert(Region {
        macro_region,
        zone_presence: presence_for_disk(macro_region, distance),
        zone_available: 0,
        distance,
    });
    Ok(())
}

/// Set `zone_available` bit `bit` for `macro_region`, effective from `time_ms`
/// ONWARD. No-op if no region governs the zone at/before `time_ms`.
///
/// Set `zone_available` bit `bit` for `macro_region` — call when a zone's row is
/// created. Current-value: read the one region row, OR the bit, update in place.
/// No-op if no region governs the zone or the bit is already set.
fn set_available_bit(ctx: &ReducerContext, macro_region: u64, bit: u8) {
    let mask = 1u64 << bit;
    let Some(mut region) = get_region(ctx, macro_region) else {
        return;
    };
    if region.zone_available & mask != 0 {
        return;
    }
    region.zone_available |= mask;
    ctx.db.regions().macro_region().update(region);
}

/// Clear `zone_available` bit `bit` — the symmetric counterpart to
/// [`set_available_bit`], for when a zone's row is removed. (To stop the zone from
/// regenerating, also clear its `zone_presence` bit.) Unused until a zone-removal
/// path exists; kept so the create/remove pair stays visible and symmetric.
#[allow(dead_code)]
fn clear_available_bit(ctx: &ReducerContext, macro_region: u64, bit: u8) {
    let mask = 1u64 << bit;
    let Some(mut region) = get_region(ctx, macro_region) else {
        return;
    };
    if region.zone_available & mask == 0 {
        return;
    }
    region.zone_available &= !mask;
    ctx.db.regions().macro_region().update(region);
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
    // Gate-computed packed tile bytes (DSL worldgen). Used for world-surface
    // zones; non-world surfaces ignore it and seed their own rect/disk grid.
    tiles: Vec<u64>,
) -> Result<(), String> {
    let (macro_region, bit) = region_of_zone(macro_zone);
    let mask = 1u64 << bit;

    let Some(region) = get_region(ctx, macro_region) else {
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

    // About to write the zone row — resolve the client-aligned timestamp (rejects
    // on excessive drift). Only the ZONE row is timeline-stamped; the region's
    // `available` bit is current-value (set below, untimed).
    let now = crate::cards::effective_now_ms(ctx, client_time_ms)?;

    if !row_exists {
        // Tiles are computed gate-side (DSL worldgen for the world surface; an
        // empty grid for non-world rect/inventory zones) and supplied here; the
        // module just stores them. World zones are world-owned (0); other
        // surfaces are owned by their container.
        let mut arr = [0u64; crate::packed::ZONE_TILE_U64_COUNT];
        for (i, v) in tiles.iter().take(arr.len()).enumerate() {
            arr[i] = *v;
        }
        // Clip the gate-supplied content to this region's disk: zero any cell
        // whose global tile coord is farther than `distance` from the origin
        // `(0,0)`. World regions (`distance = u16::MAX`) clip nothing. Iterate the
        // LOGICAL 7×7 cells (the gate emits only those); col/row 7 stay empty.
        use crate::packed::{tile_slot, world_tile, ZONE_SIZE};
        let (zq, zr) = crate::packed::unpack_macro_zone(macro_zone);
        let d = region.distance as i32;
        for lr in 0..ZONE_SIZE {
            for lc in 0..ZONE_SIZE {
                if hex_dist(world_tile(zq, lc as u8), world_tile(zr, lr as u8), 0, 0) > d {
                    crate::packed::set_tile_full(&mut arr, tile_slot(lc as u8, lr as u8), 0, 0, 0);
                }
            }
        }
        let owner = if surface_of(macro_zone) == WORLD_LAYER { 0 } else { owner_of(macro_zone) };
        let zone_id = zones::next_zone_id(ctx);
        zones::create_at(
            ctx,
            zone_id,
            macro_zone,
            pack_zone_definition(TILE_ZONE_TYPE),
            owner,
            arr,
            now,
        );
    }

    set_available_bit(ctx, macro_region, bit);
    Ok(())
}
