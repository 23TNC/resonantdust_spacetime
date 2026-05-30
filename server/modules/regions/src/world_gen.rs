use std::sync::OnceLock;

use spacetimedb::{reducer, ReducerContext, Table};

use resonantdust_content::biome_core;
use resonantdust_content::definition_core::{
    aspect_id, cards_of_type, decode_definition, AspectId, CardDefinition, StockSlot,
};

use crate::packed::{self, pack_definition, pack_macro_zone, pack_zone_definition, valid_at_time};
// Module + the generated `zones()` accessor trait. Without the trait
// import, `ctx.db.zones()` fails to resolve outside `zones.rs` itself.
// Same pattern as `magnetic.rs` uses for the cards accessor.
use crate::zones::{self, zones as _zones_table};

/// Card type for tile defs (mirrors `content/cards/types.json` → `tile: 7`).
/// Used to combine a tile def_id with the type-nibble when packing
/// into a `packed_definition` (4-bit type + 12-bit def_id) and when
/// asking the content registry for every tile def via
/// `cards_of_type(TILE_CARD_TYPE)`.
const TILE_CARD_TYPE: u8 = 7;

// Zone-row `packed_definition` encodes which definition catalog its
// tile bytes index into. For terrain that's `tile/default` —
// `card_type = 7` ("tile"). Same id `utilities::bootstrap` uses for
// its seed zones. (The `card_category` dimension was retired — see
// docs/CATEGORY_RETIRE_AND_TILE_EXPAND.md.)
pub const TILE_ZONE_TYPE: u8 = 7;

// First world surface. Surfaces `< 64` are reserved for inventory-ish
// layers (the `q == 1` force rule in `actions.rs` and the inventory
// convention in `utilities::add_card`); world zones land at 64.
const WORLD_SURFACE: u8 = 64;

// FBM tuning — used by every climate sampler and the stock fallback
// band. Dominant low-frequency octave plus a 2× detail octave with a
// seed offset so the detail isn't aligned with the dominant lattice.
// Sum is normalized so output stays in `[0, 1)` regardless of weight
// tuning.
const FBM_OCTAVE_WEIGHTS: [f32; 2] = [1.0, 0.5];

/// Spatial scale of the per-slot fallback FBM band used by
/// `pick_stocks_for` when a stock slot omits `climate_axis`. Tuned
/// for cell-scale speckle — boulders / clumps that vary per cell
/// rather than tracking a smooth climate gradient.
const STOCK_FALLBACK_BASE_SCALE: f32 = 1.0 / 4.0;

/// Per-stock-slot seed offsets for the fallback FBM. Indexed by
/// slot 0 / slot 1. Chosen by splitmix64 of small ints so the two
/// slots' fallback bands don't share a lattice — uncorrelated stocks
/// within a tile when both slots fall back.
const STOCK_SLOT_SEED_OFFSETS: [u64; 2] = [
    0xBF58_476D_1CE4_E5B9,
    0x94D0_49BB_1331_11EB,
];

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
    let [w0, w1] = FBM_OCTAVE_WEIGHTS;
    let o0 = value_noise(x, y, seed);
    let o1 = value_noise(x * 2.0, y * 2.0, seed.wrapping_add(0x9E37_79B9_7F4A_7C15));
    (w0 * o0 + w1 * o1) / (w0 + w1)
}

// ---- climate axes ---------------------------------------------------
//
// Four orthogonal noise channels sampled per cell — each its own FBM
// with its own base scale and seed offset. Returns a value in `[0, 1)`
// per axis. The biome/tile selector in `tile_for` reads all four and
// (a) walks the biome table for biome assignment, (b) filters tile
// defs by their declared climate envelopes (`climate.*` traits), and
// (c) drives per-tile stock values for slots that declare a
// `climate_axis` coupling.
//
// **Spatial scale (`1 / lattice_period`)** controls feature size. A
// smaller value means a longer period — broader, slower-varying
// regions:
//
// - Elevation: very large (mountains span tens of cells). 1/40 → ~80
//   cells between peaks.
// - Temperature: large but faster than elevation. Loose orthogonal
//   correlation with latitude isn't modelled in v1 — sample
//   independently and revisit if mountains feel un-cold.
// - Humidity: medium. Microclimates inside a region.
// - Aether: mixed bands plus sharp local peaks for ley-line /
//   resonance sites. v1 ships a single-octave sampler; the "sharp
//   peaks" octave can layer on top later.
const ELEVATION_BASE_SCALE: f32 = 1.0 / 40.0;
const TEMPERATURE_BASE_SCALE: f32 = 1.0 / 25.0;
const HUMIDITY_BASE_SCALE: f32 = 1.0 / 12.0;
const AETHER_BASE_SCALE: f32 = 1.0 / 15.0;

// Per-axis seed offsets — XORed into `WORLD_SEED` so each axis's
// lattice is decorrelated from every other axis's. Distinct constants
// (chosen by `splitmix64` of small ints) so two axes never share a
// lattice corner.
const ELEVATION_SEED_OFFSET: u64 = 0xA341_316C_C2C7_3030;
const TEMPERATURE_SEED_OFFSET: u64 = 0x6C25_7BBA_AB47_C0DA;
const HUMIDITY_SEED_OFFSET: u64 = 0x4F49_8C77_E915_4F03;
const AETHER_SEED_OFFSET: u64 = 0x9E1A_C401_BB18_9E5F;

/// Index into [`Climate`]'s `[f32; 4]` for the elevation axis.
pub const AXIS_ELEVATION: usize = 0;
/// Index into [`Climate`]'s `[f32; 4]` for the temperature axis.
pub const AXIS_TEMPERATURE: usize = 1;
/// Index into [`Climate`]'s `[f32; 4]` for the humidity axis.
pub const AXIS_HUMIDITY: usize = 2;
/// Index into [`Climate`]'s `[f32; 4]` for the aether axis.
pub const AXIS_AETHER: usize = 3;

/// Number of climate axes. Width of [`Climate`]'s inner array.
pub const CLIMATE_AXIS_COUNT: usize = 4;

/// Four climate samples for one world hex — indexed by `AXIS_*`
/// constants. All values are `f32 ∈ [0, 1)`.
///
/// Sampled once per cell in `pick_tile` and re-used by both biome
/// assignment and `pick_stocks_for` (so the stock pass doesn't pay
/// for re-sampling the same noise lattice four times). Cheap to copy
/// — `[f32; 4]` is 16 bytes; pass by value.
pub type Climate = [f32; CLIMATE_AXIS_COUNT];

/// Sample elevation at `(global_q, global_r)`. Very large spatial
/// scale — peaks span tens of cells, valleys correspondingly broad.
pub fn sample_elevation(global_q: i32, global_r: i32, seed: u64) -> f32 {
    let x = global_q as f32 * ELEVATION_BASE_SCALE;
    let y = global_r as f32 * ELEVATION_BASE_SCALE;
    fbm(x, y, seed ^ ELEVATION_SEED_OFFSET)
}

/// Sample temperature at `(global_q, global_r)`. Large spatial scale,
/// independent of elevation in v1.
pub fn sample_temperature(global_q: i32, global_r: i32, seed: u64) -> f32 {
    let x = global_q as f32 * TEMPERATURE_BASE_SCALE;
    let y = global_r as f32 * TEMPERATURE_BASE_SCALE;
    fbm(x, y, seed ^ TEMPERATURE_SEED_OFFSET)
}

/// Sample humidity at `(global_q, global_r)`. Medium spatial scale —
/// drives microclimate variation within a region's broader
/// temperature band.
pub fn sample_humidity(global_q: i32, global_r: i32, seed: u64) -> f32 {
    let x = global_q as f32 * HUMIDITY_BASE_SCALE;
    let y = global_r as f32 * HUMIDITY_BASE_SCALE;
    fbm(x, y, seed ^ HUMIDITY_SEED_OFFSET)
}

/// Sample aether at `(global_q, global_r)`. Mixed bands today (single
/// FBM); designed to gain a sharper second octave later for ley-line
/// hot spots, hence the seed offset chosen to leave room for a second
/// octave at `+ 0x9E37...` without colliding with other axes.
pub fn sample_aether(global_q: i32, global_r: i32, seed: u64) -> f32 {
    let x = global_q as f32 * AETHER_BASE_SCALE;
    let y = global_r as f32 * AETHER_BASE_SCALE;
    fbm(x, y, seed ^ AETHER_SEED_OFFSET)
}

/// Sample all four climate axes at `(global_q, global_r)`. Convenience
/// wrapper — the tile selector and stock sampler both want every axis,
/// and bundling them up-front means we sample the noise lattice once
/// per cell instead of repeating for each callsite that needs a
/// subset.
pub fn sample_climate(global_q: i32, global_r: i32, seed: u64) -> Climate {
    [
        sample_elevation(global_q, global_r, seed),
        sample_temperature(global_q, global_r, seed),
        sample_humidity(global_q, global_r, seed),
        sample_aether(global_q, global_r, seed),
    ]
}

// ---- climate-trait id cache -----------------------------------------
//
// `aspect_id("elevation_min")` is a BTreeMap lookup on the content
// crate's trait registry — cheap but called millions of times across
// a worldgen pass (one per tile-def per cell). The OnceLock here
// resolves each climate trait id exactly once at first use; subsequent
// lookups are constant-time field reads. Panics at init if any
// climate trait is missing from `traits.json` — these are static
// authoring requirements, not runtime data, so a panic surfaces the
// authoring bug at the first procedural-gen reducer call.

struct ClimateAspectIds {
    elevation_min: AspectId,
    elevation_max: AspectId,
    temperature_min: AspectId,
    temperature_max: AspectId,
    humidity_min: AspectId,
    humidity_max: AspectId,
    aether_min: AspectId,
    aether_max: AspectId,
    rarity: AspectId,
    cluster_group: AspectId,
    cluster_strength: AspectId,
}

static CLIMATE_TRAITS: OnceLock<ClimateAspectIds> = OnceLock::new();

fn require_trait(name: &str) -> AspectId {
    aspect_id(name)
        .expect("aspect registry should build")
        .unwrap_or_else(|| {
            panic!(
                "trait-category aspect {:?} not declared in aspects.json's `traits` section",
                name
            )
        })
}

fn climate_traits() -> &'static ClimateAspectIds {
    CLIMATE_TRAITS.get_or_init(|| ClimateAspectIds {
        elevation_min: require_trait("elevation_min"),
        elevation_max: require_trait("elevation_max"),
        temperature_min: require_trait("temperature_min"),
        temperature_max: require_trait("temperature_max"),
        humidity_min: require_trait("humidity_min"),
        humidity_max: require_trait("humidity_max"),
        aether_min: require_trait("aether_min"),
        aether_max: require_trait("aether_max"),
        rarity: require_trait("rarity"),
        cluster_group: require_trait("cluster_group"),
        cluster_strength: require_trait("cluster_strength"),
    })
}

// ---- biome classification -------------------------------------------

/// Canonical world seed. Both terrain generation and the synthetic-hex
/// revert-on-consumption path (`action_completion::apply` calling
/// [`biome_for`]) key off this value, so a consumed tile reverts back
/// to the same biome the noise originally placed there.
///
/// **Picked deliberately** — at this value the origin `(0, 0)` and
/// the radius-4 disk around it land 100% inside the forest envelope,
/// while the wider radius-24 ring carries a mix (~9% mountain, ~8%
/// desert, ~34% forest, ~50% plains). Gives the spawn area a
/// consistent biome to dial in forest content against while keeping
/// the other biomes reachable for testing. Re-find via a noise
/// scanner if the climate sampler constants change — env/biome
/// envelopes shift the per-seed origin classification.
///
/// **Limitation:** if `generate_forest_terrain` is called with a seed
/// other than `WORLD_SEED`, the revert won't agree with the generated
/// tiles. The public reducer takes a seed argument for testing /
/// future multi-world support; for the current single-world setup,
/// pass `WORLD_SEED` (which `bootstrap` does automatically). Per-zone
/// seed storage on the Zone row is the future-proof fix once
/// multi-world becomes a thing.
pub const WORLD_SEED: u64 = 0x27;

/// Resolve the "barren base" tile def_id for the world hex
/// `(global_q, global_r)`. Used by `action_completion::apply` to
/// revert a consumed synthetic-hex tile to what the biome would have
/// placed there before any variant / decorator pass.
///
/// Walks the biome registry against the cell's four climate samples
/// and returns the matched biome's `base_tile_packed` (low 12 bits).
/// If no biome envelope matches the cell, falls back to the last
/// biome in declaration order (the broadest fallback by convention —
/// `plains` today). Returns `0` if the biome registry has no biomes
/// or the matched biome has no `base_tile` declared — caller's job
/// to treat 0 as "no tile" if it ever appears.
pub fn biome_for(global_q: i32, global_r: i32) -> u16 {
    let climate = sample_climate(global_q, global_r, WORLD_SEED);
    let biomes = match biome_core::biomes() {
        Ok(b) => b,
        Err(_) => return 0,
    };
    let id =
        biome_core::biome_for_climate(climate[AXIS_ELEVATION], climate[AXIS_TEMPERATURE],
                                      climate[AXIS_HUMIDITY], climate[AXIS_AETHER])
            .unwrap_or(biome_core::BIOME_NONE);
    let biome = if id == biome_core::BIOME_NONE {
        biomes.last()
    } else {
        biomes.get((id - 1) as usize)
    };
    biome
        .and_then(|b| b.base_tile_packed)
        .map(|packed| packed & packed::DEF_ID_MASK)
        .unwrap_or(0)
}

// ---- envelope + rarity helpers --------------------------------------

/// Is the cell's climate inside the tile def's declared envelope?
///
/// Missing trait values on an axis default to the loose end of that
/// axis (min → 0.0, max → 1.0), so any axis the def doesn't
/// constrain matches everywhere. A def must declare **at least one**
/// climate trait to be a world-gen candidate, though — otherwise it
/// would match every cell.
///
/// The "at least one" gate exists so non-terrain `tile`-type cards
/// (buildings, future player-placed structures) don't pollute world
/// gen just by living in the same card-type bucket as forest /
/// plains / desert. Zones store tile rows by `card_type`, so
/// buildings can't live in their own type — instead the convention
/// is "if it has no climate envelope, it's not terrain."
fn tile_envelope_contains(def: &CardDefinition, climate: &Climate) -> bool {
    let t = climate_traits();
    let elev_min = def.aspect_value(t.elevation_min);
    let elev_max = def.aspect_value(t.elevation_max);
    let temp_min = def.aspect_value(t.temperature_min);
    let temp_max = def.aspect_value(t.temperature_max);
    let hum_min = def.aspect_value(t.humidity_min);
    let hum_max = def.aspect_value(t.humidity_max);
    let aeth_min = def.aspect_value(t.aether_min);
    let aeth_max = def.aspect_value(t.aether_max);
    let has_any_climate = elev_min.is_some()
        || elev_max.is_some()
        || temp_min.is_some()
        || temp_max.is_some()
        || hum_min.is_some()
        || hum_max.is_some()
        || aeth_min.is_some()
        || aeth_max.is_some();
    if !has_any_climate {
        return false;
    }
    climate[AXIS_ELEVATION] >= elev_min.unwrap_or(0.0)
        && climate[AXIS_ELEVATION] <= elev_max.unwrap_or(1.0)
        && climate[AXIS_TEMPERATURE] >= temp_min.unwrap_or(0.0)
        && climate[AXIS_TEMPERATURE] <= temp_max.unwrap_or(1.0)
        && climate[AXIS_HUMIDITY] >= hum_min.unwrap_or(0.0)
        && climate[AXIS_HUMIDITY] <= hum_max.unwrap_or(1.0)
        && climate[AXIS_AETHER] >= aeth_min.unwrap_or(0.0)
        && climate[AXIS_AETHER] <= aeth_max.unwrap_or(1.0)
}

/// Weight-pick a tile from `candidates`, biased by each def's
/// `placement.rarity` trait (default 1.0). Returns the picked def's
/// `definition_id` (low 12 bits — already type-stripped).
///
/// Pick is deterministic given `(global_q, global_r, seed)`: hashes
/// the cell coords into the seed, maps to `[0, 1)`, then walks the
/// candidate list summing weights. The first candidate whose running
/// weight crosses the rolled value wins.
fn weight_pick_candidate(
    candidates: &[&CardDefinition],
    global_q: i32,
    global_r: i32,
    seed: u64,
) -> u16 {
    let t = climate_traits();
    let mut total: f32 = 0.0;
    for d in candidates {
        total += d.aspect_value(t.rarity).unwrap_or(1.0).max(0.0);
    }
    if total <= 0.0 {
        // All candidates have rarity 0 (or negative — clamped to 0).
        // Fall back to the first candidate to keep the result
        // deterministic and avoid returning 0.
        return candidates
            .first()
            .map(|d| d.definition_id)
            .unwrap_or(0);
    }
    // `hash2` returns u64; truncate to u32, map to [0, total).
    let h = hash2(global_q, global_r, seed.wrapping_add(0x4D2A_3A04_4E8D_2D85)) as u32;
    let mut roll = (h as f32) / 4_294_967_296.0 * total;
    for d in candidates {
        let w = d.aspect_value(t.rarity).unwrap_or(1.0).max(0.0);
        roll -= w;
        if roll <= 0.0 {
            return d.definition_id;
        }
    }
    // Floating-point drift — fall through to the last candidate.
    candidates
        .last()
        .map(|d| d.definition_id)
        .unwrap_or(0)
}

// ---- stock sampling -------------------------------------------------

/// Sample one stock slot's value, climate-coupled or via fallback FBM.
///
/// - If `slot.climate_axis` is `Some(axis)`, read `climate[axis]`,
///   remap through `[climate_axis_min, climate_axis_max]`, and
///   quantise to `[0..=slot.max]`.
/// - Otherwise sample an independent FBM band at
///   `STOCK_FALLBACK_BASE_SCALE`, seed-offset by `slot_idx`, quantise
///   the same way.
fn pick_slot_stock(
    slot: &StockSlot,
    slot_idx: usize,
    climate: &Climate,
    global_q: i32,
    global_r: i32,
    seed: u64,
) -> u8 {
    let raw = match slot.climate_axis {
        Some(axis) => climate[axis.index()],
        None => {
            let offset = STOCK_SLOT_SEED_OFFSETS
                .get(slot_idx)
                .copied()
                .unwrap_or(0);
            let x = global_q as f32 * STOCK_FALLBACK_BASE_SCALE;
            let y = global_r as f32 * STOCK_FALLBACK_BASE_SCALE;
            fbm(x, y, seed ^ offset)
        }
    };
    quantise_to_stock(raw, slot.climate_axis_min, slot.climate_axis_max, slot.max)
}

/// Remap `raw ∈ [0, 1]` through the window `[lo, hi]` (clamping at
/// the ends) and quantise to `[0..=max]`. A degenerate window
/// (`lo >= hi`) collapses everything to 0 — parser already rejects
/// that for `climate_axis` slots, so this only matters for safety on
/// the fallback path (where `lo = 0.0`, `hi = 1.0` always).
fn quantise_to_stock(raw: f32, lo: f32, hi: f32, max: u8) -> u8 {
    if max == 0 || hi <= lo {
        return 0;
    }
    let remapped = ((raw - lo) / (hi - lo)).clamp(0.0, 1.0);
    let steps = (max as f32) + 1.0;
    let v = (remapped * steps).floor() as i32;
    v.clamp(0, max as i32) as u8
}

/// Build the `(stock0, stock1)` tuple for a tile def at world hex
/// `(global_q, global_r)`. Each declared slot samples per
/// `pick_slot_stock`; absent slots return 0.
fn pick_stocks_for(
    def: &CardDefinition,
    climate: &Climate,
    global_q: i32,
    global_r: i32,
    seed: u64,
) -> (u8, u8) {
    let s0 = def
        .stock
        .first()
        .map(|s| pick_slot_stock(s, 0, climate, global_q, global_r, seed))
        .unwrap_or(0);
    let s1 = def
        .stock
        .get(1)
        .map(|s| pick_slot_stock(s, 1, climate, global_q, global_r, seed))
        .unwrap_or(0);
    (s0, s1)
}

// ---- cluster bias ---------------------------------------------------
//
// Tile defs can declare `placement.cluster_group` (a small integer
// id) and `placement.cluster_strength` (in [0, 1]). When picking a
// cell's tile, candidates with a non-zero cluster_group get a weight
// boost proportional to how many of the cell's 6 neighbours would
// pick a tile in the same cluster_group.
//
// Neighbour picks are computed via `pick_tile_base` — the same
// envelope-filter + rarity-weighted pick, but WITHOUT the cluster
// step. That breaks the infinite recursion ("to know my cluster
// bonus, I need my neighbours; to know theirs, I'd need theirs…")
// while keeping the result deterministic: a cell's outcome depends
// only on its own climate and its neighbours' base picks, both
// pure functions of `(q, r, seed)`.
//
// The cluster bias is further *spatially gated* by a low-frequency
// FBM "cluster mask". Without the mask, the bias is viral — any cell
// with ≥1 group-matching neighbour gets a weight boost that beats
// base rarity, so clumps spread until they hit a wall of
// no-matching-neighbour cells. That's typically zone-scale or
// larger. The mask carves the world into "cluster-on" patches (where
// bias fires) and "cluster-off" gaps (where pure rarity wins) at a
// configurable spatial scale.

/// Hex axial offsets for the 6 immediate neighbours of `(q, r)`.
const HEX_NEIGHBOURS: [(i32, i32); 6] = [
    (1, 0), (-1, 0),
    (0, 1), (0, -1),
    (1, -1), (-1, 1),
];

/// Spatial scale of the cluster-bias mask FBM. `1 / period`, so
/// `1/3` → lattice period ~6 cells. Smaller denominator (`1/8`,
/// `1/12`) gives broader cluster regions (zone-scale); larger
/// denominator (`1/2`) gives finer, more frequent clumps.
const CLUSTER_MASK_BASE_SCALE: f32 = 1.0 / 3.0;

/// Per-axis seed offset so the cluster-mask lattice is decorrelated
/// from every climate axis and the stock-fallback bands.
const CLUSTER_MASK_SEED_OFFSET: u64 = 0xB4A4_D3EF_8C29_1E5A;

/// Cells with `cluster_mask(q, r) < threshold` skip the cluster
/// bonus entirely and fall back to pure rarity. Raise → fewer /
/// smaller cluster regions; lower → broader. FBM mean is ~0.5; at
/// `0.55` roughly 45% of the world is cluster-on, with smooth-edged
/// patches of `~CLUSTER_MASK_BASE_SCALE^-1` cells across.
const CLUSTER_MASK_THRESHOLD: f32 = 0.55;

/// Sample the cluster-bias mask at `(global_q, global_r)`. Returns
/// `f32 ∈ [0, 1)`. Cells whose value exceeds [`CLUSTER_MASK_THRESHOLD`]
/// have cluster bias active; the rest use pure rarity.
fn cluster_mask(global_q: i32, global_r: i32, seed: u64) -> f32 {
    let x = global_q as f32 * CLUSTER_MASK_BASE_SCALE;
    let y = global_r as f32 * CLUSTER_MASK_BASE_SCALE;
    fbm(x, y, seed ^ CLUSTER_MASK_SEED_OFFSET)
}

/// Base tile pick (no cluster bias). Used by the cluster-bias step on
/// the focal cell to compute what each neighbour would pick. Returns
/// just the def_id — stocks aren't needed for the cluster signal.
fn pick_tile_base(global_q: i32, global_r: i32, seed: u64) -> u16 {
    let climate = sample_climate(global_q, global_r, seed);
    let all_tiles = match cards_of_type(TILE_CARD_TYPE) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let candidates: Vec<&CardDefinition> = all_tiles
        .into_iter()
        .filter(|d| tile_envelope_contains(d, &climate))
        .collect();
    if candidates.is_empty() {
        return biome_for(global_q, global_r);
    }
    weight_pick_candidate(&candidates, global_q, global_r, seed)
}

/// Read the `cluster_group` of each of `(q, r)`'s six neighbours.
/// `0` for "neighbour's pick declares no cluster group" or for
/// neighbours that fail to resolve.
fn neighbour_cluster_groups(global_q: i32, global_r: i32, seed: u64) -> [u8; 6] {
    let t = climate_traits();
    let mut out = [0u8; 6];
    for (i, (dq, dr)) in HEX_NEIGHBOURS.iter().enumerate() {
        let def_id = pick_tile_base(global_q + dq, global_r + dr, seed);
        if def_id == 0 {
            continue;
        }
        let packed = pack_definition(TILE_CARD_TYPE, def_id);
        if let Ok(Some(ndef)) = decode_definition(packed) {
            out[i] = ndef.aspect_value(t.cluster_group).unwrap_or(0.0) as u8;
        }
    }
    out
}

/// Cluster-aware weighted pick. Combines `placement.rarity` with a
/// per-candidate boost `1 + cluster_strength * matching_neighbours`,
/// then weight-picks the same way [`weight_pick_candidate`] does.
///
/// **Spatially gated** by [`cluster_mask`]: cells whose mask value
/// falls below [`CLUSTER_MASK_THRESHOLD`] skip the bonus entirely
/// and pick by bare rarity. This carves the world into cluster-on
/// patches (where group-matching neighbours dominate) and
/// cluster-off gaps (where rarity wins), preventing viral spread of
/// the cluster across whole regions.
///
/// Candidates without a cluster_group keep their bare rarity weight
/// regardless of the mask.
fn weight_pick_with_cluster(
    candidates: &[&CardDefinition],
    neighbour_groups: &[u8; 6],
    global_q: i32,
    global_r: i32,
    seed: u64,
) -> u16 {
    let t = climate_traits();
    let cluster_active =
        cluster_mask(global_q, global_r, seed) >= CLUSTER_MASK_THRESHOLD;
    let weights: Vec<f32> = candidates
        .iter()
        .map(|d| {
            let rarity = d.aspect_value(t.rarity).unwrap_or(1.0).max(0.0);
            if !cluster_active {
                return rarity;
            }
            let group = d.aspect_value(t.cluster_group).unwrap_or(0.0) as u8;
            if group == 0 {
                return rarity;
            }
            let strength = d.aspect_value(t.cluster_strength).unwrap_or(0.0).max(0.0);
            if strength <= 0.0 {
                return rarity;
            }
            let matching = neighbour_groups.iter().filter(|g| **g == group).count() as f32;
            rarity * (1.0 + strength * matching)
        })
        .collect();

    let total: f32 = weights.iter().sum();
    if total <= 0.0 {
        return candidates
            .first()
            .map(|d| d.definition_id)
            .unwrap_or(0);
    }
    let h = hash2(global_q, global_r, seed.wrapping_add(0x4D2A_3A04_4E8D_2D85)) as u32;
    let mut roll = (h as f32) / 4_294_967_296.0 * total;
    for (i, w) in weights.iter().enumerate() {
        roll -= w;
        if roll <= 0.0 {
            return candidates[i].definition_id;
        }
    }
    candidates
        .last()
        .map(|d| d.definition_id)
        .unwrap_or(0)
}

// ---- tile selection -------------------------------------------------

/// Pick a tile for the given world hex — biome-driven selection plus
/// climate-coupled stock values, with cluster bias on candidates that
/// declare a `placement.cluster_group`.
///
/// Returns `(def_id, stock0, stock1)` where `def_id` is the low 12
/// bits of the tile's `packed_definition` (the value stored in the
/// per-tile u16 slot of the Zone row).
///
/// Flow per cell:
/// 1. Sample the four climate axes.
/// 2. Filter every tile def by its climate envelope (`climate.*`
///    traits). A def with no envelope passes for every cell.
/// 3. Compute each neighbour's *base* tile pick (envelope + rarity,
///    no cluster) so the focal pick knows how many neighbours share
///    each candidate's cluster_group.
/// 4. Weight-pick by `rarity * (1 + cluster_strength * matching_neighbours)`.
/// 5. Sample each declared stock slot, climate-coupled or fallback
///    FBM per the slot's `climate_axis`.
///
/// If no tile def matches the cell's climate (envelopes are over-
/// constrained), falls back to the biome's `base_tile_packed`
/// (`biome_for(q, r)`) with zero stocks. If even that's unresolvable,
/// returns `(0, 0, 0)` — the renderer treats def_id 0 as empty.
pub fn pick_tile(global_q: i32, global_r: i32, seed: u64) -> (u16, u8, u8) {
    let climate = sample_climate(global_q, global_r, seed);
    let all_tiles = match cards_of_type(TILE_CARD_TYPE) {
        Ok(v) => v,
        Err(_) => return (0, 0, 0),
    };
    let candidates: Vec<&CardDefinition> = all_tiles
        .into_iter()
        .filter(|d| tile_envelope_contains(d, &climate))
        .collect();
    if candidates.is_empty() {
        let fallback = biome_for(global_q, global_r);
        return (fallback, 0, 0);
    }
    let neighbour_groups = neighbour_cluster_groups(global_q, global_r, seed);
    let def_id = weight_pick_with_cluster(
        &candidates,
        &neighbour_groups,
        global_q,
        global_r,
        seed,
    );
    let def = candidates
        .iter()
        .find(|d| d.definition_id == def_id)
        .copied();
    let (s0, s1) = match def {
        Some(d) => pick_stocks_for(d, &climate, global_q, global_r, seed),
        None => (0, 0),
    };
    (def_id, s0, s1)
}

// ---- zone packer -----------------------------------------------------

/// Build the 16 packed tile-u64s for one zone at macro coord
/// `(macro_q, macro_r)`. Pure function — no DB access — so the same
/// `(macro_q, macro_r, seed)` triple always yields the same bytes.
///
/// Each zone covers an 8×8 patch of world hexes; the zone at macro
/// coord `(Q, R)` starts at world hex `(Q*8, R*8)`. Climate noise is
/// sampled in world-hex coordinates, so biome boundaries span zone
/// boundaries instead of restarting at each zone's edge.
pub fn generate_zone_tiles(
    macro_q: i16,
    macro_r: i16,
    seed: u64,
) -> [u64; packed::ZONE_TILE_U64_COUNT] {
    let base_q = macro_q as i32 * 8;
    let base_r = macro_r as i32 * 8;
    let mut packed = [0u64; packed::ZONE_TILE_U64_COUNT];
    for r in 0..8u8 {
        let mut row = [(0u16, 0u8, 0u8); 8];
        for c in 0..8u8 {
            let (def_id, s0, s1) =
                pick_tile(base_q + c as i32, base_r + r as i32, seed);
            row[c as usize] = (def_id, s0, s1);
        }
        packed::set_tile_row(&mut packed, r as usize, &row);
    }
    packed
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
///   `surface = 64`, `packed_definition = pack_zone_definition(
///   card_type=7)` (tile), and a freshly-allocated `zone_id`.
///   Allocation walks `max(zone_id) + 1` at reducer entry and
///   increments locally — fine while the table is small; promote to
///   a counter table if the zone history grows hot.
///
/// Then for each forest hex in each zone, [`forest_object_for`] rolls
/// for a tree (40%), rock (10%), or nothing (50%). Hits become world
/// cards owned by [`WORLD_OWNER_ID`] — the auto-created World
/// pseudo-player. Card placement: `surface = 64`, `macro_zone` is the
/// zone's, `micro` is `Loose` at the tile's local `(q, r)` with zero
/// within-cell offset (centered on the hex), loose-hex kind.
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

    let zone_def = pack_zone_definition(TILE_ZONE_TYPE);

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
            // `macro_zone` carries the surface band (bits 24-31), so it's the
            // complete key for both the existing-zone filter and `create`.
            let macro_zone = crate::packed::with_surface(pack_macro_zone(q, r), WORLD_SURFACE);
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
    fn pick_tile_returns_registered_def_or_zero() {
        // pick_tile should either return a def_id that resolves in
        // the content registry (a real tile) or 0 (the "no candidate
        // and biome_for fallback also failed" sentinel — acceptable
        // for sparsely-authored climate corners). Stocks are u2.
        for q in -20..20 {
            for r in -20..20 {
                let (def_id, s0, s1) = pick_tile(q, r, 7);
                assert!(s0 <= packed::ZONE_TILE_STOCK_MAX, "stock0 {s0} out of range");
                assert!(s1 <= packed::ZONE_TILE_STOCK_MAX, "stock1 {s1} out of range");
                if def_id != 0 {
                    let packed = crate::packed::pack_definition(TILE_CARD_TYPE, def_id);
                    assert!(
                        resonantdust_content::definition_core::decode_definition(packed)
                            .ok()
                            .flatten()
                            .is_some(),
                        "pick_tile returned unknown def_id {def_id} at ({q}, {r})"
                    );
                }
            }
        }
    }

    #[test]
    fn zone_tiles_pack_round_trip() {
        let packed_tiles = generate_zone_tiles(0, 0, 42);
        // Sanity: every slot decodes back to (def_id, stock0, stock1)
        // with stocks in [0, ZONE_TILE_STOCK_MAX]. def_id is the
        // 12-bit field — content-validity is covered by
        // `pick_tile_returns_registered_def_or_zero`.
        for idx in 0..packed::ZONE_TILE_COUNT {
            let (def_id, s0, s1) = packed::tile_full(&packed_tiles, idx);
            let _ = def_id;
            assert!(s0 <= packed::ZONE_TILE_STOCK_MAX, "stock0 {s0} out of range");
            assert!(s1 <= packed::ZONE_TILE_STOCK_MAX, "stock1 {s1} out of range");
        }
    }

    #[test]
    fn climate_samples_in_unit_range() {
        // Every climate axis must produce values in [0, 1) — the
        // envelope match and stock quantise both assume it.
        for q in -50..50 {
            for r in -50..50 {
                let c = sample_climate(q, r, WORLD_SEED);
                for (idx, v) in c.iter().enumerate() {
                    assert!(
                        (0.0..1.0).contains(v),
                        "climate axis {} out of range at ({}, {}): {}",
                        idx, q, r, v
                    );
                }
            }
        }
    }
}
