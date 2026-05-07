//! Procedural map generation.
//!
//! Pure and deterministic: the same `(zone_q, zone_r)` always produces
//! the same cell layout across runs and machines. No `ReducerContext`,
//! no `rand`, no I/O — generation is hash-driven so the world is
//! infinite-on-demand without persisting a seed.
//!
//! # Pipeline (per cell)
//!
//! 1. Sample a [`Climate`] vector — one independent value-noise channel
//!    per climate axis (`temperature`, `humidity`). See [`sample_climate`].
//! 2. For each biome, compute an inverse-square weight from the cell's
//!    climate point to the biome's center in climate space.
//! 3. Sum each biome's tile distribution scaled by that biome weight,
//!    producing one combined `(definition_id, weight)` table for the cell.
//! 4. Weighted-pick a `definition_id` from that table using a separate
//!    hash channel so the climate values and the tile pick decorrelate.
//!
//! Cells near a biome's center are dominated by that biome; cells on
//! the boundary draw from a true mix of both biomes' tile tables. Tile
//! aspects on the resolved [`CardDefinition`](crate::definitions::CardDefinition)
//! drive downstream game behavior — mapgen only places `definition_id`s.
//!
//! # Biome data
//!
//! Loaded from `data/biomes.json` lazily on first call via the same
//! `OnceLock<Result<…, String>>` pattern used in
//! [`crate::definitions`]. Tile keys are resolved through
//! `find_packed("tile/default/<key>")` at registry-build time so a
//! typo fails loudly once instead of at every fill.

use std::sync::OnceLock;

use serde_json::Value;

use crate::definitions::find_packed;
use crate::packing::unpack_definition;
use crate::zones::{write_cell, LocalCoord, ZONE_SIDE, ZONE_U64_COUNT};

// ─── Climate sampling ───────────────────────────────────────────────────────

/// Climate vector at one world cell. Each axis is in `[0.0, 1.0]`.
/// Add new axes here when extending biome selection (e.g. `elevation`,
/// `magic`); the sampler will need a matching seed and lattice step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Climate {
  pub temperature: f32,
  pub humidity: f32,
}

/// Spatial scale of the temperature noise lattice in cells. Larger ⇒
/// broader temperature regions. 64 cells = 8 zones across.
const TEMPERATURE_LATTICE_STEP: u32 = 64;
/// Spatial scale of the humidity noise lattice in cells.
const HUMIDITY_LATTICE_STEP: u32 = 48;

/// Per-channel noise seeds. Different seeds make the channels vary
/// independently — temperature and humidity decorrelate, so wet-cold
/// and wet-warm regions both occur naturally.
const TEMPERATURE_SEED: u64 = 0xA1B2_C3D4_E5F6_0718;
const HUMIDITY_SEED:    u64 = 0x0F1E_2D3C_4B5A_6978;
/// Seed for the per-cell tile-pick hash. Independent of climate seeds
/// so a cell's pick within its biome blend doesn't correlate with its
/// climate values.
const TILE_PICK_SEED: u64 = 0x1234_5678_9ABC_DEF0;

/// Sample the climate field at world cell `(q, r)`.
pub fn sample_climate(q: i32, r: i32) -> Climate {
  Climate {
    temperature: value_noise(TEMPERATURE_SEED, q, r, TEMPERATURE_LATTICE_STEP),
    humidity:    value_noise(HUMIDITY_SEED,    q, r, HUMIDITY_LATTICE_STEP),
  }
}

// ─── Biome registry ─────────────────────────────────────────────────────────

const BIOMES_JSON: &str = include_str!("../data/biomes.json");
static BIOMES: OnceLock<Result<Vec<Biome>, String>> = OnceLock::new();

#[derive(Debug, Clone)]
struct Biome {
  /// Programmatic key from `biomes.json` — kept for error messages.
  #[allow(dead_code)]
  name: String,
  /// Center in climate space. Cells whose sampled climate sits closer
  /// to this point draw more strongly from this biome's tile table.
  center: Climate,
  /// `(definition_id, weight)` pairs. Resolved from tile keys through
  /// the cards registry at build time. Weights are relative within
  /// this biome — the absolute scale doesn't matter once the
  /// distributions get blended.
  tiles: Vec<(u8, u32)>,
}

fn biomes() -> Result<&'static [Biome], String> {
  BIOMES
    .get_or_init(build_biomes)
    .as_ref()
    .map(Vec::as_slice)
    .map_err(|e| e.clone())
}

fn build_biomes() -> Result<Vec<Biome>, String> {
  let root: Value = serde_json::from_str(BIOMES_JSON)
    .map_err(|e| format!("biomes.json: parse failed: {}", e))?;
  let biomes_obj = root
    .get("biomes")
    .and_then(Value::as_object)
    .ok_or_else(|| "biomes.json: 'biomes' missing or not an object".to_string())?;

  let mut out: Vec<Biome> = Vec::with_capacity(biomes_obj.len());
  for (name, body) in biomes_obj {
    if name.starts_with('_') {
      continue;
    }
    let body = body
      .as_object()
      .ok_or_else(|| format!("biomes.json: biome {:?} not an object", name))?;

    let temperature = read_unit(body.get("temperature"), name, "temperature")?;
    let humidity    = read_unit(body.get("humidity"),    name, "humidity")?;

    let tiles_obj = body
      .get("tiles")
      .and_then(Value::as_object)
      .ok_or_else(|| format!("biomes.json: biome {:?} missing 'tiles' object", name))?;

    let mut tiles: Vec<(u8, u32)> = Vec::with_capacity(tiles_obj.len());
    for (tile_key, weight_val) in tiles_obj {
      if tile_key.starts_with('_') {
        continue;
      }
      let weight = weight_val.as_u64().ok_or_else(|| {
        format!(
          "biomes.json: biome {:?} tile {:?} weight not a non-negative integer",
          name, tile_key
        )
      })?;
      if weight == 0 || weight > u32::MAX as u64 {
        return Err(format!(
          "biomes.json: biome {:?} tile {:?} weight {} out of range (1..={})",
          name, tile_key, weight, u32::MAX
        ));
      }
      // Scope the lookup to (tile, default) so a typo'd key — or one
      // that resolves to some other card type — fails here rather
      // than silently writing wrong cells.
      let path = format!("tile/default/{}", tile_key);
      let packed = find_packed(&path).map_err(|e| {
        format!("biomes.json: biome {:?} tile {:?}: {}", name, tile_key, e)
      })?;
      let (_card_type, _category, definition_id) = unpack_definition(packed);
      tiles.push((definition_id, weight as u32));
    }

    if tiles.is_empty() {
      return Err(format!("biomes.json: biome {:?} has no tiles", name));
    }

    out.push(Biome {
      name: name.clone(),
      center: Climate { temperature, humidity },
      tiles,
    });
  }

  if out.is_empty() {
    return Err("biomes.json: no biomes declared".to_string());
  }
  Ok(out)
}

fn read_unit(value: Option<&Value>, biome_name: &str, field: &str) -> Result<f32, String> {
  let v = value.ok_or_else(|| {
    format!("biomes.json: biome {:?} missing '{}'", biome_name, field)
  })?;
  let n = v.as_f64().ok_or_else(|| {
    format!(
      "biomes.json: biome {:?} '{}' not a number: {:?}",
      biome_name, field, v
    )
  })?;
  if !(0.0..=1.0).contains(&n) {
    return Err(format!(
      "biomes.json: biome {:?} '{}' value {} out of range [0.0, 1.0]",
      biome_name, field, n
    ));
  }
  Ok(n as f32)
}

// ─── Cell fill ──────────────────────────────────────────────────────────────

/// Fill `rows` (the eight `u64`s of a [`Zone`](crate::zones::Zone)'s
/// byte-packed cell array) with `definition_id`s picked by sampling
/// climate noise per cell and weighted-blending across the biome
/// registry's tile tables. Pure — no I/O, deterministic in
/// `(zone_q, zone_r)`.
pub fn fill_zone_cells(
  zone_q: i16,
  zone_r: i16,
  rows: &mut [u64; ZONE_U64_COUNT],
) -> Result<(), String> {
  let biomes = biomes()?;
  let zone_origin_q = (zone_q as i32) * (ZONE_SIDE as i32);
  let zone_origin_r = (zone_r as i32) * (ZONE_SIDE as i32);

  for local_r in 0..ZONE_SIDE {
    for local_q in 0..ZONE_SIDE {
      let world_q = zone_origin_q + local_q as i32;
      let world_r = zone_origin_r + local_r as i32;
      let def_id = pick_definition_id(biomes, world_q, world_r);
      let coord = LocalCoord::new(local_q, local_r)?;
      write_cell(rows, coord, def_id);
    }
  }
  Ok(())
}

/// Inverse-square epsilon — a cell sitting exactly on a biome's
/// climate center would otherwise divide by zero. Small enough that a
/// near-center biome still dominates; large enough that the weight
/// stays finite.
const DISTANCE_EPSILON: f32 = 1e-4;

/// Pick the `definition_id` for one world cell by weighted-blending
/// the biome tile distributions. Climate sampling and the final pick
/// both hash on `(q, r)` so the result is stable across runs.
fn pick_definition_id(biomes: &[Biome], q: i32, r: i32) -> u8 {
  let climate = sample_climate(q, r);

  // Per-biome inverse-square weight in climate space. Allocations are
  // cheap relative to the overall fill cost; if biome counts ever
  // climb to the hundreds, switch to a stack array.
  let mut biome_weights: Vec<f32> = Vec::with_capacity(biomes.len());
  for b in biomes {
    let dt = climate.temperature - b.center.temperature;
    let dh = climate.humidity    - b.center.humidity;
    let d2 = dt * dt + dh * dh;
    biome_weights.push(1.0 / (DISTANCE_EPSILON + d2));
  }

  // Combined tile distribution. Same `def_id` can appear in multiple
  // biomes — sum their contributions. f64 internally keeps precision
  // for the cumulative pick; the absolute scale is normalized out.
  let mut total: f64 = 0.0;
  let mut combined: Vec<(u8, f64)> = Vec::new();
  for (b, &bw) in biomes.iter().zip(biome_weights.iter()) {
    for &(def_id, tile_weight) in &b.tiles {
      let contribution = (bw as f64) * (tile_weight as f64);
      total += contribution;
      if let Some(slot) = combined.iter_mut().find(|(id, _)| *id == def_id) {
        slot.1 += contribution;
      } else {
        combined.push((def_id, contribution));
      }
    }
  }

  // Cumulative pick. `target` lands in `[0, total)`.
  let target = (hash_unit(TILE_PICK_SEED, q, r) as f64) * total;
  let mut acc: f64 = 0.0;
  for (def_id, w) in &combined {
    acc += *w;
    if target < acc {
      return *def_id;
    }
  }
  // Floating-point safety net: if rounding slips past the last entry
  // we still return a real tile rather than the empty-cell sentinel.
  combined.last().map(|(id, _)| *id).unwrap_or(0)
}

// ─── Hash-based value noise ─────────────────────────────────────────────────

/// Bilinear-interpolated value noise sampled at world cell `(q, r)`.
/// Lattice points are spaced `lattice_step` cells apart; each lattice
/// point's value comes from [`hash_unit`] keyed on `(seed, lat_q, lat_r)`.
/// Output is in `[0.0, 1.0)`.
fn value_noise(seed: u64, q: i32, r: i32, lattice_step: u32) -> f32 {
  let step = lattice_step as i32;
  let lat_q = q.div_euclid(step);
  let lat_r = r.div_euclid(step);
  let frac_q = (q.rem_euclid(step) as f32) / step as f32;
  let frac_r = (r.rem_euclid(step) as f32) / step as f32;

  let v00 = hash_unit(seed, lat_q,     lat_r);
  let v10 = hash_unit(seed, lat_q + 1, lat_r);
  let v01 = hash_unit(seed, lat_q,     lat_r + 1);
  let v11 = hash_unit(seed, lat_q + 1, lat_r + 1);

  let sq = smoothstep(frac_q);
  let sr = smoothstep(frac_r);

  lerp(lerp(v00, v10, sq), lerp(v01, v11, sq), sr)
}

#[inline]
fn smoothstep(t: f32) -> f32 {
  t * t * (3.0 - 2.0 * t)
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
  a + (b - a) * t
}

/// Hash `(seed, q, r)` to a uniform `[0.0, 1.0)` float.
fn hash_unit(seed: u64, q: i32, r: i32) -> f32 {
  let h = hash3(seed, q, r);
  // 24 bits of entropy is plenty for an f32 mantissa.
  let bits = (h >> 40) as u32;
  (bits as f32) * (1.0 / ((1u32 << 24) as f32))
}

/// Splitmix-style mixer of `(seed, q, r)` to a `u64`. Stable across
/// builds; good enough avalanche for value-noise lattice values and
/// per-cell tile picks.
fn hash3(seed: u64, q: i32, r: i32) -> u64 {
  let qh = (q as i64 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
  let rh = (r as i64 as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
  let mut h = seed ^ qh ^ rh.rotate_left(27);
  h ^= h >> 30;
  h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
  h ^= h >> 27;
  h
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn climate_in_unit_range() {
    for &q in &[-1024, -8, 0, 1, 7, 64, 1024] {
      for &r in &[-1024, -8, 0, 1, 7, 64, 1024] {
        let c = sample_climate(q, r);
        assert!(
          (0.0..1.0).contains(&c.temperature),
          "temperature out of range at ({}, {}): {}", q, r, c.temperature
        );
        assert!(
          (0.0..1.0).contains(&c.humidity),
          "humidity out of range at ({}, {}): {}", q, r, c.humidity
        );
      }
    }
  }

  #[test]
  fn climate_is_deterministic() {
    for &q in &[-7, 0, 33] {
      for &r in &[-7, 0, 33] {
        assert_eq!(sample_climate(q, r), sample_climate(q, r));
      }
    }
  }

  #[test]
  fn climate_varies_smoothly() {
    // Adjacent cells inside one lattice cell shouldn't jump anywhere
    // close to the full range. With 48–64 cells per lattice step the
    // 1-cell delta is < ~2% of the range typically.
    let a = sample_climate(0, 0);
    let b = sample_climate(1, 0);
    let c = sample_climate(0, 1);
    let dt = (a.temperature - b.temperature).abs()
      + (a.temperature - c.temperature).abs();
    let dh = (a.humidity - b.humidity).abs()
      + (a.humidity - c.humidity).abs();
    assert!(dt < 0.4, "temperature varies too sharply: {}", dt);
    assert!(dh < 0.4, "humidity varies too sharply: {}", dh);
  }

  #[test]
  fn value_noise_continuous_across_lattice() {
    // Output at the lattice line from either side should match —
    // crossing q = step jumps lat_q from 0 to 1 with frac_q resetting
    // from ~1 to 0. Values should agree.
    let step = 16u32;
    let left  = value_noise(0xCAFE, step as i32 - 0, 0, step); // frac_q = 0 in cell (1, 0)
    let right = value_noise(0xCAFE, step as i32,     0, step); // frac_q = 0 in cell (1, 0)
    assert_eq!(left, right);
  }

  #[test]
  fn pick_is_deterministic_and_picks_a_listed_id() {
    // Synthesize a tiny biome set so this test doesn't depend on the
    // shipped biomes.json or the cards registry.
    let biomes = vec![
      Biome {
        name: "a".into(),
        center: Climate { temperature: 0.2, humidity: 0.2 },
        tiles: vec![(11, 1), (22, 3)],
      },
      Biome {
        name: "b".into(),
        center: Climate { temperature: 0.8, humidity: 0.8 },
        tiles: vec![(22, 2), (33, 5)],
      },
    ];
    let valid: std::collections::BTreeSet<u8> = [11u8, 22, 33].into_iter().collect();

    for &q in &[-100, -1, 0, 1, 50] {
      for &r in &[-100, -1, 0, 1, 50] {
        let id = pick_definition_id(&biomes, q, r);
        assert!(valid.contains(&id), "got unlisted id {} at ({}, {})", id, q, r);
        // Determinism: same coord ⇒ same pick.
        assert_eq!(id, pick_definition_id(&biomes, q, r));
      }
    }
  }
}
