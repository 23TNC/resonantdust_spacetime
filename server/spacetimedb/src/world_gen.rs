use spacetimedb::{reducer, ReducerContext, Table};

use crate::packed::{pack_macro_zone, pack_tiles, pack_zone_definition, valid_at_time};
// Module + the generated `zones()` accessor trait. Without the trait
// import, `ctx.db.zones()` fails to resolve outside `zones.rs` itself.
// Same pattern as `magnetic.rs` uses for the cards accessor.
use crate::zones::{self, zones as _zones_table};

// Tile def_ids inside the `tile/default` bucket. Source of truth is
// `content/cards/id.json` — kept as bare constants here because the
// content crate doesn't currently expose per-tile lookups, and the
// values are stable enough that duplicating them is cheaper than
// plumbing a runtime resolver. Promote to a content-side helper when
// the tile set grows.
//
// `tree` and `rock` are *tile* variants of forest — not separate
// card rows. The old tile_object-card path used those names for
// per-hex card spawns; we now express the same scenery as a tile
// byte, which keeps zone data dense (1 byte per hex) and avoids the
// per-card overhead for static scenery.
const TILE_PLAINS: u8 = 1;
const TILE_FOREST: u8 = 2;
const TILE_TREE: u8 = 3;
const TILE_ROCK: u8 = 4;

// Zone-row `packed_definition` encodes which definition catalog its
// tile bytes index into. For terrain that's `tile/default` —
// `card_type = 7` ("tile") and `card_category = 0` ("default"). Same
// pair `utilities::bootstrap` uses for its seed zones.
const TILE_ZONE_TYPE: u8 = 7;
const TILE_ZONE_CATEGORY: u8 = 0;

// First world surface. Surfaces `< 64` are reserved for inventory-ish
// layers (the `q == 1` force rule in `actions.rs` and the inventory
// convention in `utilities::add_card`); world zones land at 64.
const WORLD_SURFACE: u8 = 64;

// Noise tuning. Forest classification samples a 2-octave fractional
// Brownian motion (FBM) of value noise — a dominant low-frequency
// octave that decides the broad blob shape, plus a higher-frequency
// octave that breaks up the edges and punches small gaps / islands
// into otherwise homogenous regions. Single-octave noise at this
// scale produces "potato" blobs with smooth edges; the second octave
// is what makes the output look like a forest rather than a paint
// pour.
//
// - `FOREST_BASE_SCALE` is `1 / lattice_period` for the dominant
//   octave. At `1/5` the period is ~5 hex cells, so individual
//   forest groups land around 10-20 cells.
// - `FOREST_OCTAVE_WEIGHTS` weights `[dominant, detail]`. Detail
//   octave samples at 2× frequency. Sum is normalized so output
//   stays in `[0, 1)`.
// - `FOREST_THRESHOLD` is the cutoff above which a hex becomes
//   forest. ~0.58 keeps the forest fraction in the 30–35% range,
//   which makes plains dominant and groups feel distinct.
//
// Tuning: bigger blobs → raise `FOREST_BASE_SCALE`'s denominator
// (e.g. `1/7`). More ragged edges → raise the detail octave's
// weight. Fewer / smaller groups → raise `FOREST_THRESHOLD`.
const FOREST_BASE_SCALE: f32 = 1.0 / 5.0;
const FOREST_OCTAVE_WEIGHTS: [f32; 2] = [1.0, 0.5];
const FOREST_THRESHOLD: f32 = 0.58;

// ---- value noise primitive ------------------------------------------

/// Deterministic 64-bit hash from 2D integer coords + seed. Combination
/// of two large odd multipliers (one per axis) and a splitmix-style
/// finalizer — sufficient for value-noise lattice corners. Not
/// cryptographic.
fn hash2(x: i32, y: i32, seed: u64) -> u64 {
    // Sign-extend i32 → u64 so negative coordinates hash distinctly
    // from their positive counterparts (`-1` and `0xFFFF_FFFF` must
    // produce different hashes).
    let mut h = seed ^ ((x as i64 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    h = h.wrapping_add((y as i64 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h
}

/// Value at a lattice corner, mapped to `[0.0, 1.0)`. Truncates the
/// high 32 bits of the hash and divides by `2^32`.
fn lattice_value(x: i32, y: i32, seed: u64) -> f32 {
    let h = hash2(x, y, seed) as u32;
    (h as f32) / 4_294_967_296.0
}

/// Cubic smoothstep `t*t*(3 - 2t)`. Gives C¹ continuity at the lattice
/// corners (the noise is differentiable across cell boundaries) — the
/// reason value noise looks like rolling terrain rather than a quilt.
fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Bilinear value-noise sample at a continuous `(x, y)` point. Reads
/// the four surrounding lattice corners, smoothsteps the fractional
/// position, and bilerps.
fn value_noise(x: f32, y: f32, seed: u64) -> f32 {
    let xi = x.floor() as i32;
    let yi = y.floor() as i32;
    let xf = x - xi as f32;
    let yf = y - yi as f32;
    let a = lattice_value(xi, yi, seed);
    let b = lattice_value(xi + 1, yi, seed);
    let c = lattice_value(xi, yi + 1, seed);
    let d = lattice_value(xi + 1, yi + 1, seed);
    let u = smoothstep(xf);
    let v = smoothstep(yf);
    let ab = a + u * (b - a);
    let cd = c + u * (d - c);
    ab + v * (cd - ab)
}

/// 2-octave fractional Brownian motion of value noise. The dominant
/// octave samples at `(x, y)`; the detail octave samples at `(2x, 2y)`
/// with a seed-offset so it isn't aligned with the dominant lattice
/// (would otherwise create visible lattice-corner artifacts on the
/// edges of blobs). Weighted by `FOREST_OCTAVE_WEIGHTS`, then
/// normalized so the output stays in `[0, 1)` regardless of weight
/// tuning.
fn fbm(x: f32, y: f32, seed: u64) -> f32 {
    let [w0, w1] = FOREST_OCTAVE_WEIGHTS;
    let o0 = value_noise(x, y, seed);
    let o1 = value_noise(x * 2.0, y * 2.0, seed.wrapping_add(0x9E37_79B9_7F4A_7C15));
    (w0 * o0 + w1 * o1) / (w0 + w1)
}

// ---- biome classification -------------------------------------------

/// Canonical world seed. Both terrain generation and the synthetic-hex
/// revert-on-consumption path (`action_completion::apply` calling
/// [`biome_for`]) key off this value, so a consumed tree/rock reverts
/// back to the biome the noise originally placed there.
///
/// **Limitation:** if `generate_forest_terrain` is called with a seed
/// other than `WORLD_SEED`, the revert won't agree with the generated
/// tiles. The public reducer takes a seed argument for testing /
/// future multi-world support; for the current single-world setup, pass
/// `WORLD_SEED` (which `bootstrap` does automatically). Per-zone seed
/// storage on the Zone row is the future-proof fix once multi-world
/// becomes a thing.
pub const WORLD_SEED: u64 = 0xF05E_5700_DEAD_BEEF;

/// Biome layer only — what `tile_for` would return *before* the
/// per-tile variant roll. Pure function of `(global_q, global_r)`
/// keyed off [`WORLD_SEED`]. Returns one of `TILE_FOREST` (inside a
/// forest blob) or `TILE_PLAINS` (outside).
///
/// Used by `action_completion::apply` to revert a consumed
/// synthetic-hex tile back to its underlying biome (e.g., a chopped
/// tree leaves a forest tile, not an empty hex) — same noise the
/// generator used, so the revert is byte-equivalent to what
/// `generate_forest_terrain` originally placed before the variant
/// roll.
pub fn biome_for(global_q: i32, global_r: i32) -> u8 {
    let x = global_q as f32 * FOREST_BASE_SCALE;
    let y = global_r as f32 * FOREST_BASE_SCALE;
    if fbm(x, y, WORLD_SEED) <= FOREST_THRESHOLD {
        TILE_PLAINS
    } else {
        TILE_FOREST
    }
}

/// Pick a tile `def_id` for the given world-hex coordinate.
///
/// Two-stage classification:
/// 1. **Biome.** A 2-octave FBM thresholded at `FOREST_THRESHOLD`
///    splits the world into forest blobs (above) and plains (below).
///    The dominant octave fixes blob size (~10-20 tiles at the
///    current `FOREST_BASE_SCALE`); the detail octave punches small
///    gaps / islands into the blobs so the output reads as forest
///    rather than solid color fill.
/// 2. **Forest variant.** Inside a forest blob, an independent
///    per-tile hash picks between plain forest (50%), tree (40%),
///    and rock (10%). Trees and rocks are *tile variants* of forest
///    — not separate card rows — so the whole biome lives in the
///    8-byte tile rows of the Zone, no per-tile card overhead.
///
/// `seed` is kept as a parameter on `tile_for` (rather than reading
/// `WORLD_SEED` like [`biome_for`]) so the public
/// `generate_forest_terrain` reducer can pass through whatever seed
/// the caller supplied. Test code can run alternate seeds.
///
/// When more biomes land (water, mountain, desert, …) the body
/// becomes a layered classifier: sample one noise channel per axis
/// (moisture, elevation, temperature, …), fold them into a biome
/// pick, return the matching tile id. The signature stays.
pub fn tile_for(global_q: i32, global_r: i32, seed: u64) -> u8 {
    let x = global_q as f32 * FOREST_BASE_SCALE;
    let y = global_r as f32 * FOREST_BASE_SCALE;
    if fbm(x, y, seed) <= FOREST_THRESHOLD {
        return TILE_PLAINS;
    }
    // Inside a forest blob — roll for the scenery variant. Uses a
    // hash channel orthogonal to the biome FBM (different seed
    // offset, no spatial smoothing) so adjacent forest hexes get
    // independent rolls — visually: a forest blob doesn't lock-step
    // into "all trees" or "all rocks", it speckles. Buckets `% 100`:
    //
    // - `0..40` → tree (40%)
    // - `40..50` → rock (10%)
    // - `50..100` → plain forest tile (50%)
    //
    // Plain-forest tiles are intentional — visually, the blob reads
    // as forest because of the *tile* color; dotting only half the
    // tiles with trees / rocks keeps the world legible at zone-zoom-out.
    let h = hash2(global_q, global_r, seed.wrapping_add(0xA5A5_5A5A_DEAD_BEEF));
    let bucket = (h as u32) % 100;
    if bucket < 40 {
        TILE_TREE
    } else if bucket < 50 {
        TILE_ROCK
    } else {
        TILE_FOREST
    }
}

// ---- zone packer -----------------------------------------------------

/// Build the 8 packed tile rows for one zone at macro coord
/// `(macro_q, macro_r)`. Pure function — no DB access — so the same
/// `(macro_q, macro_r, seed)` triple always yields the same bytes.
///
/// Each zone covers an 8×8 patch of world hexes; the zone at macro
/// coord `(Q, R)` starts at world hex `(Q*8, R*8)`. Noise is sampled
/// in world-hex coordinates, so biome blobs span zone boundaries
/// instead of restarting at each zone's edge.
pub fn generate_zone_tiles(macro_q: i16, macro_r: i16, seed: u64) -> [u64; 8] {
    let base_q = macro_q as i32 * 8;
    let base_r = macro_r as i32 * 8;
    let mut rows = [0u64; 8];
    for r in 0..8u8 {
        let mut row = [TILE_PLAINS; 8];
        for c in 0..8u8 {
            row[c as usize] = tile_for(base_q + c as i32, base_r + r as i32, seed);
        }
        rows[r as usize] = pack_tiles(row);
    }
    rows
}

// ---- reducer ---------------------------------------------------------

/// Generate forest/plains terrain — and scatter tree / rock cards on a
/// subset of the resulting forest tiles — for every zone in a hex disk
/// of `radius` around macro coord `(0, 0)`.
///
/// For each `(q, r)` in the disk:
/// - **Zone exists at that macro coord**: a new `valid_at` version row
///   is written with the regenerated tile bytes. `zone_id` is kept.
/// - **No zone there yet**: a fresh zone is created with
///   `surface = 64`, `packed_definition = (card_type=7,
///   card_category=0)` (tile/default), and a freshly-allocated
///   `zone_id`. Allocation walks `max(zone_id) + 1` at reducer entry
///   and increments locally — fine while the table is small; promote
///   to a counter table if the zone history grows hot.
///
/// Then for each forest hex in each zone, [`forest_object_for`] rolls
/// for a tree (40%), rock (10%), or nothing (50%). Hits become world
/// cards owned by [`WORLD_OWNER_ID`] — the auto-created World
/// pseudo-player. Card placement: `surface = 64`, `macro_zone` is the
/// zone's, `micro_zone` packs the tile's local `(q, r)` with
/// `StackedState::Free`, `micro_location = 0` (no sub-hex pixel
/// offset — centered on the hex).
///
/// **Idempotent on re-run.** Tile bytes are pure functions of
/// `(seed, q, r)`, so re-running with the same seed regenerates
/// identical zone rows. Existing zones get a new `valid_at` version
/// row; fresh coords get a new `zone_id`. No card rows are touched —
/// trees and rocks live inside the tile bytes themselves.
///
/// `seed` is the noise seed — same `(seed, q, r)` always yields the
/// same tile. Different seeds produce different worlds.
#[reducer]
pub fn generate_forest_terrain(
    ctx: &ReducerContext,
    seed: u64,
    radius: i16,
) -> Result<(), String> {
    if radius < 0 {
        return Err("radius must be >= 0".to_string());
    }

    let zone_def = pack_zone_definition(TILE_ZONE_TYPE, TILE_ZONE_CATEGORY);

    // Allocate zone_ids starting from max+1 at reducer entry. The
    // history-style schema means `iter()` returns every version row,
    // so this is O(N) over all zone rows — fine until the zone table
    // grows large. When it does, mirror the `CardIdCounter` /
    // `PlayerIdCounter` pattern.
    let mut next_zone_id = ctx
        .db
        .zones()
        .iter()
        .map(|z| z.zone_id)
        .max()
        .unwrap_or(0)
        .saturating_add(1);

    // Hex disk in axial coords: `|q|, |r|, |q + r| <= radius`. The
    // inner-loop bounds derive that constraint for each `q`.
    for q in -radius..=radius {
        let r_min = std::cmp::max(-radius, -q - radius);
        let r_max = std::cmp::min(radius, -q + radius);
        for r in r_min..=r_max {
            let macro_zone = pack_macro_zone(q, r);
            let tiles = generate_zone_tiles(q, r, seed);

            // Existing zone at this macro coord? Use the latest
            // version row's `zone_id` and write a new version with
            // the regenerated tiles. Otherwise allocate a fresh id.
            let existing = ctx
                .db
                .zones()
                .macro_zone()
                .filter(macro_zone)
                .max_by_key(|z| valid_at_time(z.valid_at));
            match existing {
                Some(z) => {
                    zones::set_tile_rows(ctx, z.zone_id, tiles);
                }
                None => {
                    zones::create(
                        ctx,
                        next_zone_id,
                        WORLD_SURFACE,
                        macro_zone,
                        zone_def,
                        /* owner_id */ 0,
                        tiles,
                    );
                    next_zone_id = next_zone_id.saturating_add(1);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_deterministic() {
        let a = value_noise(1.7, 2.3, 42);
        let b = value_noise(1.7, 2.3, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn noise_changes_with_seed() {
        let a = value_noise(1.7, 2.3, 42);
        let b = value_noise(1.7, 2.3, 43);
        assert!(a != b, "different seeds should produce different samples");
    }

    #[test]
    fn tile_for_returns_known_tile_id() {
        for q in -20..20 {
            for r in -20..20 {
                let t = tile_for(q, r, 7);
                assert!(
                    matches!(t, TILE_PLAINS | TILE_FOREST | TILE_TREE | TILE_ROCK),
                    "unexpected tile id {t} at ({q}, {r})"
                );
            }
        }
    }

    #[test]
    fn zone_tiles_pack_round_trip() {
        let rows = generate_zone_tiles(0, 0, 42);
        // Sanity: every byte in every row is a known tile id.
        for row in rows {
            for byte in row.to_le_bytes() {
                assert!(matches!(
                    byte,
                    TILE_PLAINS | TILE_FOREST | TILE_TREE | TILE_ROCK
                ));
            }
        }
    }
}
