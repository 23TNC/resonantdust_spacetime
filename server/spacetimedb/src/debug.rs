//! Debug-only reducers for development. These bypass the normal game
//! flow (no authentication, no recipe checks) and are intended only for
//! seeding dev databases. The validation that
//! [`crate::cards::insert_card_row`] performs (layer check, target-player
//! existence) and that [`crate::definitions`] performs (real card name)
//! still applies, so a typo can't slip a bogus row through.
//!
//! # Helpers vs. reducers
//!
//! [`spawn_card_for_player`] is the underlying card-spawn routine; it's
//! shared between the [`debug_spawn`] one-off reducer and the
//! [`bootstrap`] dev-seed reducer below. New dev tooling that wants to
//! create cards programmatically should call this helper directly
//! rather than chaining reducers.
//!
//! [`bootstrap`] generates four world zones around the origin via
//! [`mapgen::fill_zone_cells`] and reads `data/bootstrap/bootstrap.json`
//! (embedded at compile time) to populate:
//!
//! - the inventory of player_id `1` with one card per entry in `"card"`.
//!   Players are created via `claim_or_login`; bootstrap assumes the
//!   first player to log in gets id 1.
//!
//! The reducer is *idempotent on zones* (replace-on-insert) and
//! *additive on cards* — re-running it adds another set of cards to
//! player 1's inventory.

use serde_json::Value;
use spacetimedb::{reducer, ReducerContext, Table};

use crate::cards::{insert_card_row, insert_card_row_at_position, Card, LAYER_INVENTORY, LAYER_WORLD, MICRO_ZONE_LOCAL_Q_ONE};
use crate::definitions::find_packed_by_key;
use crate::mapgen;
use crate::packing::pack_world_macro_zone;
use crate::zones::{self, zones as _, LocalCoord, Zone, ZONE_SIDE};

const BOOTSTRAP_JSON: &str = include_str!("../data/bootstrap/bootstrap.json");

// ─── Card spawn helper ──────────────────────────────────────────────────────

/// Spawn one card with key `card_key` into `player_id`'s inventory.
/// Resolves the key against `cards/id.json` so a typo or missing card
/// returns a descriptive error rather than producing an undecodable row.
/// The new `card_id` is auto-assigned; clients learn it via subscription.
///
/// Sets both `macro_zone` and `owner_id` to `player_id`. Real
/// card-creation paths set those independently (e.g. recipe products
/// landing in someone else's panel); this helper is for dev seeding
/// only.
///
/// Shared between [`debug_spawn`] and [`bootstrap`].
pub fn spawn_card_for_player(
  ctx: &ReducerContext,
  player_id: u32,
  card_key: &str,
) -> Result<Card, String> {
  let packed_definition = find_packed_by_key(card_key)?
    .ok_or_else(|| format!("unknown card key {:?}", card_key))?;
  insert_card_row(ctx, LAYER_INVENTORY, player_id, player_id, packed_definition)
}

// ─── debug_spawn reducer ────────────────────────────────────────────────────

/// Spawn a single card into a player's inventory. Thin wrapper over
/// [`spawn_card_for_player`] — kept as a reducer for one-off dev calls
/// from the SpacetimeDB CLI / explorer.
///
/// `card_key` is the bare key from `cards/id.json` (e.g. `"corpus"`).
/// For paths like `"discipline/attack"`, use the registry's
/// `find_packed` from a dedicated reducer instead — this one only
/// handles bare keys to match the bootstrap JSON shape.
#[reducer]
pub fn debug_spawn(
  ctx: &ReducerContext,
  player_id: u32,
  card_key: String,
) -> Result<(), String> {
  spawn_card_for_player(ctx, player_id, &card_key)?;
  Ok(())
}

// ─── debug_spawn_world reducer ─────────────────────────────────────────────

/// Spawn a world card at hex position `(world_q, world_r)`. Zone and
/// local coords are derived automatically:
///
/// ```text
/// zone_q  = world_q.div_euclid(ZONE_SIDE)   local_q = world_q.rem_euclid(ZONE_SIDE)
/// zone_r  = world_r.div_euclid(ZONE_SIDE)   local_r = world_r.rem_euclid(ZONE_SIDE)
/// ```
///
/// The card is placed loose (`stack_state = 0`) with an authoritative
/// position signal so the client trusts the placement. `micro_location`
/// is zeroed (hex center). Dev use only.
#[reducer]
pub fn debug_spawn_world(
  ctx: &ReducerContext,
  player_id: u32,
  card_key: String,
  world_q: i16,
  world_r: i16,
) -> Result<(), String> {
  let packed_definition = find_packed_by_key(&card_key)?
    .ok_or_else(|| format!("unknown card key {:?}", card_key))?;

  let side = ZONE_SIDE as i16;
  let zone_q = world_q.div_euclid(side);
  let zone_r = world_r.div_euclid(side);
  let local_q = world_q.rem_euclid(side) as u8;
  let local_r = world_r.rem_euclid(side) as u8;

  let macro_zone = pack_world_macro_zone(zone_q, zone_r);
  let coord = LocalCoord::new(local_q, local_r)?;
  // Loose placement (stack_state = 0) with local_q = 1 so the client
  // trusts the inbound position. coord.to_micro_zone() packs local_q/r
  // into bits 7..2; OR with MICRO_ZONE_LOCAL_Q_ONE sets the trust bit.
  let micro_zone = MICRO_ZONE_LOCAL_Q_ONE | coord.to_micro_zone();

  insert_card_row_at_position(ctx, LAYER_WORLD, macro_zone, player_id, packed_definition, micro_zone, 0)?;
  Ok(())
}

// ─── bootstrap reducer ──────────────────────────────────────────────────────

/// Seed the database from `data/bootstrap/bootstrap.json`. Generates
/// four world zones around the origin and appends the listed cards to
/// player_id 1's inventory.
///
/// Zones are replaced wholesale on every call. Cards are appended — re-running
/// adds another set. Players are created via `claim_or_login`; bootstrap
/// assumes the first player to log in received id 1.
#[reducer]
pub fn bootstrap(ctx: &ReducerContext) -> Result<(), String> {
  let root: Value = serde_json::from_str(BOOTSTRAP_JSON)
    .map_err(|e| format!("bootstrap.json: parse failed: {}", e))?;
  let root_obj = root
    .as_object()
    .ok_or_else(|| "bootstrap.json: top-level not an object".to_string())?;

  // ── Zones ──
  // Generate a 2×2 block of world zones surrounding the origin.
  // card_type=7 (tile), category=0 (default).
  for (zone_q, zone_r) in [(0i16, 0i16), (-1, 0), (0, -1), (-1, -1)] {
    generate_zone(ctx, zone_q, zone_r, 7, 0)?;
  }

  // ── Cards ──
  if let Some(card_arr) = root_obj.get("card").and_then(Value::as_array) {
    for entry in card_arr {
      let key = entry.as_str().ok_or_else(|| {
        format!("bootstrap.json: card entry not a string: {:?}", entry)
      })?;
      spawn_card_for_player(ctx, 1, key)?;
    }
  }

  Ok(())
}

// ─── bootstrap helpers ──────────────────────────────────────────────────────

/// Insert (or replace) one Zone row generated from `(zone_q, zone_r)`.
///
/// Cell content is filled by [`mapgen::fill_zone_cells`], which samples
/// climate noise per cell and weighted-blends across the biomes declared
/// in `data/biomes.json`. Layer is always `LAYER_WORLD`. Replace-on-insert
/// keyed on `(layer, macro_zone)` — re-running with the same pair
/// overwrites whatever was there.
fn generate_zone(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  card_type: u8,
  category: u8,
) -> Result<(), String> {
  let layer = LAYER_WORLD;
  let macro_zone = pack_world_macro_zone(zone_q, zone_r);
  let packed_definition = (card_type << 4) | category;

  let mut rows = [0u64; zones::ZONE_U64_COUNT];
  mapgen::fill_zone_cells(zone_q, zone_r, &mut rows)?;

  // Replace any existing Zone at (layer, macro_zone). Idempotent.
  if let Some(existing) = zones::find_zone(ctx, layer, macro_zone) {
    ctx.db.zones().zone_id().delete(&existing.zone_id);
  }
  ctx.db.zones().insert(Zone {
    zone_id: 0,
    layer,
    macro_zone,
    packed_definition,
    t0: rows[0], t1: rows[1], t2: rows[2], t3: rows[3],
    t4: rows[4], t5: rows[5], t6: rows[6], t7: rows[7],
    delta_t: crate::delta_t::current(),
  });
  Ok(())
}

