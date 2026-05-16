use std::sync::OnceLock;

use resonantdust_content::definition_core::{
    decode_definition, trait_id as resolve_trait_id, TraitId,
};
use spacetimedb::{reducer, ReducerContext, SpacetimeType};

use crate::cards;
use crate::packed::{
    pack_definition, pack_macro_zone, pack_micro_zone, unpack_macro_zone,
    unpack_micro_zone, valid_at_time, StackedState,
};
use crate::players;
use crate::zones::zones as _zones_table;

// ---- tuning ---------------------------------------------------------

/// Default soul speed when the soul's definition carries no `speed`
/// trait. `speed` is measured in **cost units per second** — a soul
/// with `speed: 10` traverses 10 cost-units of terrain every second,
/// so a cost-10 tile takes 1 second, a cost-100 tile takes 10
/// seconds. At `speed: 1` the same terrain takes 10× longer.
const DEFAULT_SOUL_SPEED: f32 = 10.0;

// ---- pathfinding helpers ------------------------------------------
//
// A* itself moved to the client ([pixijs/src/game/world/pathfind.ts]);
// the helpers below stay because the server-side validator
// (`move_soul_path`) reuses them per-step. See
// [docs/MOVEMENT_REWRITE.md](../../../../../docs/MOVEMENT_REWRITE.md).

/// Global axial-hex coordinate. `q = macro_q * 8 + local_q`, same for
/// `r` — flattens out the zone-level layout so A* doesn't care about
/// zone boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Coord {
    q: i32,
    r: i32,
}

/// Hex distance (cube-derived) — admissible heuristic when paired
/// with the min step cost. Returns the number of hex steps between
/// two coords ignoring tile costs.
fn hex_distance(a: Coord, b: Coord) -> i32 {
    ((a.q - b.q).abs() + (a.r - b.r).abs() + ((a.q + a.r) - (b.q + b.r)).abs()) / 2
}

/// Look up the tile def_id at a global hex coord on the given
/// `surface`. Returns `None` when no Zone covers that macro coord
/// (unmapped area) — A* treats `None` as impassable.
///
/// **Mini_zone overlay.** On the world surface (`surface == WORLD_LAYER`),
/// any deployed mini_zone whose radius-3 footprint covers this hex
/// takes priority over the underlying world tile. The mini_zone's
/// tile byte is returned; if it's `0` (empty cell of the mini_zone),
/// the hex is treated as impassable (`None`) — the mini_zone occludes
/// whatever's underneath. This matches `actions::derive_synthetic_hex`'s
/// resolver so pathfinding and recipe-target lookups agree.
fn tile_def_at(ctx: &ReducerContext, surface: u8, c: Coord) -> Option<u16> {
    let macro_q = c.q.div_euclid(8);
    let macro_r = c.r.div_euclid(8);
    let local_q = c.q.rem_euclid(8) as u8;
    let local_r = c.r.rem_euclid(8) as u8;
    let macro_zone = pack_macro_zone(macro_q as i16, macro_r as i16);

    if surface == crate::packed::WORLD_LAYER {
        let micro_zone = pack_micro_zone(local_q, local_r, crate::packed::StackedState::OnHex);
        if let Some(anchor) = crate::mini_zone::anchor_covering_hex(ctx, macro_zone, micro_zone) {
            // Mini_zone covers this hex. Read its tile byte at the
            // corresponding `(q, r)` within the mini_zone's grid.
            let zone =
                crate::zones::latest_for(ctx, crate::packed::MINI_ZONE_LAYER, anchor.card_id)?;
            let (anchor_local_q, anchor_local_r, _) =
                unpack_micro_zone(anchor.micro_zone);
            let (anchor_macro_q, anchor_macro_r) = unpack_macro_zone(anchor.macro_zone);
            let dq = c.q - (anchor_macro_q as i32 * 8 + anchor_local_q as i32);
            let dr = c.r - (anchor_macro_r as i32 * 8 + anchor_local_r as i32);
            // Mini_zone grid is centered at (3, 3) in 7×7 layout.
            let mz_q = (3 + dq) as u8;
            let mz_r = (3 + dr) as u8;
            let def_id = zone.tile_at(mz_r, mz_q)?;
            // `0` = empty mini_zone cell. Treat as impassable — the
            // mini_zone occludes the underlying world tile.
            return if def_id == 0 { None } else { Some(def_id) };
        }
    }

    let zone = ctx
        .db
        .zones()
        .macro_zone()
        .filter(macro_zone)
        .filter(|z| z.surface == surface)
        .max_by_key(|z| valid_at_time(z.valid_at))?;
    zone.tile_at(local_r, local_q)
}

/// `card_type` of the tile-card bucket. Tiles live under `tile/` in
/// `content/cards/data/tiles/*.json` — `forest_1`, `forest_2`,
/// `tree`, `rock`. Re-encoded via `pack_definition(TILE_CARD_TYPE,
/// def_id)` to look up a tile's `CardDefinition` from its u8 def_id
/// (Phase 2 of the category-retire / tile-expand rewrite will widen
/// the per-tile field to u12).
const TILE_CARD_TYPE: u8 = 7;

/// `cost` trait id, resolved once via `trait_id("cost")` and cached.
/// Lazy-init avoids paying the trait-registry build on every
/// pathfinding call (registry builds once per process via
/// `OnceLock`, but the `BTreeMap` lookup still has a cost). `0` is
/// the `TRAIT_NONE` sentinel — if the trait isn't declared in
/// `traits.json`, the cache stays at `0` and `tile_cost` falls back
/// to `DEFAULT_TILE_COST`.
static COST_TRAIT_ID: OnceLock<TraitId> = OnceLock::new();
fn cost_trait_id() -> TraitId {
    *COST_TRAIT_ID.get_or_init(|| resolve_trait_id("cost").ok().flatten().unwrap_or(0))
}

/// `speed` trait id, same cache pattern as [`cost_trait_id`]. Soul
/// definitions carry this trait (see `content/cards/data/souls/*.json`)
/// to set their movement rate. Falls back to `0` when the trait isn't
/// declared, in which case `soul_speed` returns `DEFAULT_SOUL_SPEED`.
static SPEED_TRAIT_ID: OnceLock<TraitId> = OnceLock::new();
fn speed_trait_id() -> TraitId {
    *SPEED_TRAIT_ID.get_or_init(|| resolve_trait_id("speed").ok().flatten().unwrap_or(0))
}

/// Fallback when a tile def carries no `cost` trait. Cost is measured
/// in the same units as `speed` (see [`DEFAULT_SOUL_SPEED`]) — `10`
/// matches the catalog's plains baseline (`forest_1`), so an
/// unannotated tile costs one second at default soul speed.
const DEFAULT_TILE_COST: f32 = 10.0;

/// Traversal cost of a tile, as a multiplier into the `step_cost`
/// formula. `def_id == 0` (empty / cleared tile) is impassable;
/// every other def_id resolves through the tile-card bucket to a
/// `CardDefinition` and reads its `cost` trait. Authors annotate
/// terrain difficulty in `content/cards/data/tiles/*.json` —
/// `forest_1: cost = 1`, `forest_2: cost = 1.2`, `tree`/`rock:
/// cost = 2`, etc. A def with no `cost` trait falls back to
/// `DEFAULT_TILE_COST`; an unresolvable def_id (registry build
/// failure, or a def_id not present in the catalog) is treated as
/// impassable since the cost is undefined.
fn tile_cost(def_id: u16) -> Option<f32> {
    if def_id == 0 {
        return None;
    }
    let packed = pack_definition(TILE_CARD_TYPE, def_id);
    let def = decode_definition(packed).ok().flatten()?;
    Some(def.trait_value(cost_trait_id()).unwrap_or(DEFAULT_TILE_COST))
}

/// Read a soul card's `speed` trait (in cost-units-per-second),
/// falling back to `DEFAULT_SOUL_SPEED` when the trait isn't set
/// (or the definition can't be decoded).
fn soul_speed(packed_def: u16) -> f32 {
    decode_definition(packed_def)
        .ok()
        .flatten()
        .and_then(|def| def.trait_value(speed_trait_id()))
        .unwrap_or(DEFAULT_SOUL_SPEED)
}

/// Time (in seconds, before rounding to whole `valid_at` seconds) for
/// a single hex step from a tile of cost `from` to a tile of cost
/// `to` at the given `speed`. Splits the cost equally between leaving
/// the current tile and entering the next — each contributes half its
/// cost — then divides by `speed` (cost-units-per-second). Net:
/// `step_secs = (from/2 + to/2) / speed`. Uniform cost-10 terrain at
/// speed 10 = 1 sec/hex; speed 20 halves that; cost 100 at speed 10
/// = 10 sec/hex.
fn step_cost(speed: f32, from: f32, to: f32) -> f32 {
    0.5 * (from + to) / speed
}


// ---- reducer --------------------------------------------------------

/// Cap on the number of steps the client-submitted path may contain.
/// DoS guard: each step costs one zone btree lookup + a tile-cost
/// resolve + a per-step row write, so the validation walk is linear
/// in path length. 256 is generous for plausible cross-zone treks
/// (8×8 tiles per zone, multiple zones) while keeping worst-case
/// reducer time well under a millisecond. Tune against measured
/// path lengths once a real load profile exists.
const MAX_VALIDATION_STEPS: usize = 256;

/// One step in a client-submitted path. Mirrors the
/// `(surface, macro_zone, micro_zone)` triplet the soul row already
/// carries — same packing, same `micro_zone` `state == OnHex`
/// requirement. `micro_location` isn't on the wire (world-hex
/// placements always use `0`).
///
/// Per-step `surface` is carried so the wire format stays
/// forward-compatible with cross-surface pathing (portals, etc.).
/// Today the validator rejects any path where a step's `surface`
/// differs from the soul's current surface.
#[derive(SpacetimeType, Debug, Clone, Copy)]
pub struct TilePoint {
    pub surface: u8,
    pub macro_zone: u32,
    pub micro_zone: u8,
}

/// Move the caller's soul along a client-computed path.
///
/// The client runs A* against its `data.zonesLocal` mirror
/// (see [pixijs/src/game/world/pathfind.ts]) and submits the
/// resulting tile sequence here; the server validates adjacency +
/// traversability + length and queues the per-step row writes.
/// See [docs/MOVEMENT_REWRITE.md](../../../../../docs/MOVEMENT_REWRITE.md).
///
/// Validation per step:
///   - `surface` matches the soul's current surface (cross-surface
///     transitions not supported).
///   - `micro_zone`'s state bits == `OnHex`.
///   - Axially adjacent to the predecessor (the soul's current tile
///     for step 0; the previous step otherwise).
///   - The tile is traversable — `tile_def_at` returns a def_id and
///     `tile_cost` returns `Some(_)`. Empty tiles (`def_id == 0`) and
///     impassable defs (no `cost` trait resolves) reject the path.
///
/// On any validation failure the reducer aborts before any row
/// writes — partial paths don't get queued. On success the queue
/// loop matches the cost / scheduling discipline the old
/// server-side A* `move_soul` produced (same `step_cost` formula,
/// same `valid_at` strictly-ascending clamp, same
/// `scrub_or_repath_position_forward` cleanup).
///
/// **Interrupts.** A second `move_soul` call before the first
/// path's last step lands invokes
/// [`cards::scrub_or_repath_position_forward`] below: pure-position
/// queued steps get DELETED; data-bearing rows have their position
/// fields re-homed to the soul's `latest` row; `position_preserve`
/// rows (recipe pins / magnetic anchors) stop the walk.
#[reducer]
pub fn move_soul(
    ctx: &ReducerContext,
    soul_id: u32,
    path: Vec<TilePoint>,
) -> Result<(), String> {
    let player_id = players::resolve_caller(ctx)?;
    let soul = cards::latest(ctx, soul_id)
        .ok_or_else(|| format!("movement: soul card {soul_id} not found"))?;
    if soul.owner_id != player_id {
        return Err(format!(
            "movement: soul card {soul_id} is owned by player {} (not {player_id})",
            soul.owner_id
        ));
    }

    if path.is_empty() {
        return Err("movement: empty path".to_string());
    }
    if path.len() > MAX_VALIDATION_STEPS {
        return Err(format!(
            "movement: path length {} exceeds cap {MAX_VALIDATION_STEPS}",
            path.len()
        ));
    }

    // Decode the soul's current tile — this is the implicit step-0
    // predecessor for the adjacency walk.
    let (s_lq, s_lr, _) = unpack_micro_zone(soul.micro_zone);
    let (s_mq, s_mr) = unpack_macro_zone(soul.macro_zone);
    let mut prev = Coord {
        q: s_mq as i32 * 8 + s_lq as i32,
        r: s_mr as i32 * 8 + s_lr as i32,
    };

    let speed = soul_speed(soul.packed_definition);

    // First pass: full validation walk. Resolve every step's tile cost
    // up-front so we don't write rows for a path that turns out to be
    // invalid halfway through.
    //
    // Per-step costs are stored alongside the decoded coord so the
    // write loop below doesn't repeat the zone lookups. Two passes
    // because the write loop needs the cost of the PREVIOUS tile too
    // (for `step_cost`), and stashing both with each step keeps the
    // loop straightforward.
    let mut decoded: Vec<(TilePoint, Coord, f32)> = Vec::with_capacity(path.len());
    let start_cost = tile_def_at(ctx, soul.surface, prev)
        .and_then(tile_cost)
        .ok_or_else(|| {
            format!(
                "movement: soul's current tile ({}, {}) is not traversable",
                prev.q, prev.r,
            )
        })?;
    let mut prev_cost = start_cost;

    for (idx, &point) in path.iter().enumerate() {
        if point.surface != soul.surface {
            return Err(format!(
                "movement: step {idx} surface {} differs from soul surface {} (cross-surface not supported)",
                point.surface, soul.surface,
            ));
        }
        let (lq, lr, state) = unpack_micro_zone(point.micro_zone);
        if state != StackedState::OnHex {
            return Err(format!(
                "movement: step {idx} micro_zone state must be OnHex (got {state:?})"
            ));
        }
        let (mq, mr) = unpack_macro_zone(point.macro_zone);
        let coord = Coord {
            q: mq as i32 * 8 + lq as i32,
            r: mr as i32 * 8 + lr as i32,
        };
        if hex_distance(prev, coord) != 1 {
            return Err(format!(
                "movement: step {idx} ({}, {}) not axially adjacent to predecessor ({}, {})",
                coord.q, coord.r, prev.q, prev.r,
            ));
        }
        let to_def = tile_def_at(ctx, point.surface, coord).ok_or_else(|| {
            format!(
                "movement: step {idx} ({}, {}) — no zone data (off-map or subscription gap server-side)",
                coord.q, coord.r,
            )
        })?;
        let to_cost = tile_cost(to_def).ok_or_else(|| {
            format!(
                "movement: step {idx} ({}, {}) is impassable (def_id={to_def})",
                coord.q, coord.r,
            )
        })?;
        decoded.push((point, coord, to_cost));
        prev = coord;
        prev_cost = to_cost;
    }
    // Silence unused-warning in the rare case the loop body is empty
    // (path.is_empty() is rejected above, so this is purely defensive).
    let _ = prev_cost;

    // Validation passed — scrub any future-stamped rows on the soul
    // from a prior path so the new path's writes start from a clean
    // future. Same call site / semantics as today's `move_soul`.
    let now = cards::now_ms(ctx);
    cards::scrub_or_repath_position_forward(
        ctx,
        soul.card_id,
        now,
        soul.surface,
        soul.macro_zone,
        soul.micro_zone,
        soul.micro_location,
    );

    // Write loop. Per step: accumulate elapsed time via the same
    // `step_cost` formula `move_soul` uses, force strictly-ascending
    // `valid_at` to dodge PK collisions on cheap consecutive steps.
    let mut elapsed: f32 = 0.0;
    let mut last_time = now;
    let mut from_cost = start_cost;
    for (point, _coord, to_cost) in decoded {
        elapsed += step_cost(speed, from_cost, to_cost);
        let mut step_time = now.saturating_add((elapsed * 1_000.0).ceil() as u64);
        if step_time <= last_time {
            step_time = last_time.saturating_add(1);
        }
        last_time = step_time;

        cards::update_with_at(ctx, soul.card_id, step_time, |c| {
            c.surface = point.surface;
            c.macro_zone = point.macro_zone;
            c.micro_zone = point.micro_zone;
            c.micro_location = 0;
        });

        from_cost = to_cost;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_distance_axial() {
        assert_eq!(hex_distance(Coord { q: 0, r: 0 }, Coord { q: 0, r: 0 }), 0);
        assert_eq!(hex_distance(Coord { q: 0, r: 0 }, Coord { q: 3, r: 0 }), 3);
        assert_eq!(hex_distance(Coord { q: 0, r: 0 }, Coord { q: 0, r: -3 }), 3);
        // Cube-distance triangle: (2, -1) is two steps away along q-r axis.
        assert_eq!(hex_distance(Coord { q: 0, r: 0 }, Coord { q: 2, r: -1 }), 2);
    }

    #[test]
    fn step_cost_default() {
        // speed = cost-units-per-second. Plains pair (cost=10) at
        // speed 10 = 1 sec/hex.
        assert!((step_cost(10.0, 10.0, 10.0) - 1.0).abs() < f32::EPSILON);
        // Mixed plains↔forest (cost 10 ↔ 12): half-and-half average
        // works out to 1.1 sec at speed 10.
        assert!((step_cost(10.0, 10.0, 12.0) - 1.1).abs() < f32::EPSILON);
        // Heavy tile pair (cost 30 ↔ 30) at speed 10 = 3 sec.
        assert!((step_cost(10.0, 30.0, 30.0) - 3.0).abs() < f32::EPSILON);
        // Faster soul: double the speed halves the time.
        assert!((step_cost(20.0, 10.0, 10.0) - 0.5).abs() < f32::EPSILON);
        // Slower soul: speed 1 over cost-100 terrain = 100 sec/hex
        // (the user's reference example).
        assert!((step_cost(1.0, 100.0, 100.0) - 100.0).abs() < f32::EPSILON);
    }
}
