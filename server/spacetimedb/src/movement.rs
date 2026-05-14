use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::OnceLock;

use resonantdust_content::definition_core::{
    decode_definition, trait_id as resolve_trait_id, TraitId,
};
use spacetimedb::{reducer, ReducerContext};

use crate::cards;
use crate::packed::{
    pack_definition, pack_macro_zone, pack_micro_zone, tile_byte, unpack_macro_zone,
    unpack_micro_zone, valid_at_time, StackedState,
};
use crate::players;
use crate::zones::zones as _zones_table;

// `cards/flags.json` bit position for the client-side movement-arrow
// hint. Set on every queued path-step row so the client's
// `DataManager.resolveCardTarget` recognises the row as a queued
// destination (it requires both `move_smooth` and `position_dirty`
// to mark a future row as a target). Teleport / push / recipe-pin
// writes leave it clear so they don't render an arrow.
const FLAG_MOVE_SMOOTH: u32 = 1 << 17;

// ---- tuning ---------------------------------------------------------

/// Default soul speed when the soul's definition carries no `speed`
/// trait. `speed` is measured in **cost units per second** — a soul
/// with `speed: 10` traverses 10 cost-units of terrain every second,
/// so a cost-10 tile takes 1 second, a cost-100 tile takes 10
/// seconds. At `speed: 1` the same terrain takes 10× longer.
const DEFAULT_SOUL_SPEED: f32 = 10.0;

/// Max nodes the A* expansion will visit before giving up. Bounds
/// pathological searches (target on the far side of an unreachable
/// chasm, malformed input) so the reducer doesn't burn unlimited
/// instructions. Tune against typical path lengths; a radius-2
/// bootstrap world is ~300 hexes, so 1000 leaves comfortable slack.
const MAX_PATH_NODES: usize = 1_000;

/// Axial-hex neighbor offsets (six directions). Layout convention:
/// `(dq, dr)` deltas in the same axial space the rest of the codebase
/// uses for `global_q / global_r`.
const HEX_DIRS: [(i32, i32); 6] = [
    (1, 0),
    (-1, 0),
    (0, 1),
    (0, -1),
    (1, -1),
    (-1, 1),
];

// ---- pathfinding ----------------------------------------------------

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
fn tile_def_at(ctx: &ReducerContext, surface: u8, c: Coord) -> Option<u8> {
    let macro_q = c.q.div_euclid(8);
    let macro_r = c.r.div_euclid(8);
    let local_q = c.q.rem_euclid(8) as u8;
    let local_r = c.r.rem_euclid(8) as u8;
    let macro_zone = pack_macro_zone(macro_q as i16, macro_r as i16);
    let zone = ctx
        .db
        .zones()
        .macro_zone()
        .filter(macro_zone)
        .filter(|z| z.surface == surface)
        .max_by_key(|z| valid_at_time(z.valid_at))?;
    let row_bytes = zone.tile_row(local_r)?;
    Some(tile_byte(row_bytes, local_q as usize))
}

/// `card_type` / `card_category` of the tile-card bucket. Tiles
/// live under `tile/default` in `content/cards/data/tiles/*.json` —
/// `forest_1`, `forest_2`, `tree`, `rock`. Re-encoded via
/// `pack_definition(TILE_CARD_TYPE, TILE_CARD_CATEGORY, def_id)` to
/// look up a tile's `CardDefinition` from its u8 def_id (which is
/// what's stored in zone tile bytes).
const TILE_CARD_TYPE: u8 = 7;
const TILE_CARD_CATEGORY: u8 = 0;

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
fn tile_cost(def_id: u8) -> Option<f32> {
    if def_id == 0 {
        return None;
    }
    let packed = pack_definition(TILE_CARD_TYPE, TILE_CARD_CATEGORY, def_id);
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

/// A* over the world-hex grid. Returns the path inclusive of both
/// endpoints — `[start, …, goal]` — ordered by traversal.
fn pathfind(
    ctx: &ReducerContext,
    surface: u8,
    start: Coord,
    goal: Coord,
    speed: f32,
) -> Result<Vec<Coord>, String> {
    // Heuristic scaling: minimum possible per-step cost on this
    // surface is `step_cost(speed, DEFAULT_TILE_COST, DEFAULT_TILE_COST)`
    // — i.e., 1 second at the catalog's plains baseline (cost 10,
    // speed 10). The heuristic `hex_distance * h_scale` is admissible
    // (never overestimates) provided no tile in the world is cheaper
    // than `DEFAULT_TILE_COST`. Today every catalog tile uses
    // cost ≥ 10 so that holds; if a future tile dips below, lower
    // this bound accordingly.
    let h_scale = step_cost(speed, DEFAULT_TILE_COST, DEFAULT_TILE_COST);

    let mut open: BinaryHeap<(Reverse<i64>, Coord)> = BinaryHeap::new();
    let mut came_from: BTreeMap<Coord, Coord> = BTreeMap::new();
    let mut g_score: BTreeMap<Coord, f32> = BTreeMap::new();

    g_score.insert(start, 0.0);
    open.push((Reverse(0), start));

    let mut expanded = 0usize;
    while let Some((_, current)) = open.pop() {
        if current == goal {
            // Reconstruct path from `came_from`.
            let mut path = vec![current];
            let mut cur = current;
            while let Some(&prev) = came_from.get(&cur) {
                path.push(prev);
                cur = prev;
            }
            path.reverse();
            return Ok(path);
        }
        expanded += 1;
        if expanded > MAX_PATH_NODES {
            return Err(format!(
                "movement: path search exceeded {MAX_PATH_NODES} nodes (start=({}, {}) goal=({}, {}))",
                start.q, start.r, goal.q, goal.r,
            ));
        }
        let current_g = g_score.get(&current).copied().unwrap_or(f32::INFINITY);
        let Some(curr_def) = tile_def_at(ctx, surface, current) else {
            continue;
        };
        let Some(curr_cost) = tile_cost(curr_def) else {
            continue;
        };
        for &(dq, dr) in &HEX_DIRS {
            let neighbor = Coord {
                q: current.q + dq,
                r: current.r + dr,
            };
            let Some(neigh_def) = tile_def_at(ctx, surface, neighbor) else {
                continue;
            };
            let Some(neigh_cost) = tile_cost(neigh_def) else {
                continue;
            };
            let tentative = current_g + step_cost(speed, curr_cost, neigh_cost);
            let existing = g_score.get(&neighbor).copied().unwrap_or(f32::INFINITY);
            if tentative < existing {
                came_from.insert(neighbor, current);
                g_score.insert(neighbor, tentative);
                let f = tentative + hex_distance(neighbor, goal) as f32 * h_scale;
                // BinaryHeap is a max-heap; `Reverse(i64)` flips it
                // into a min-heap. Scale by 1000 to preserve sub-
                // second cost precision through the i64 round.
                open.push((Reverse((f * 1000.0) as i64), neighbor));
            }
        }
    }
    Err(format!(
        "movement: no path from ({}, {}) to ({}, {}) on surface {surface}",
        start.q, start.r, goal.q, goal.r,
    ))
}

// ---- reducer --------------------------------------------------------

/// Move the caller's soul card to the tile at `(target_surface,
/// target_macro_zone, target_micro_zone)`.
///
/// `target_micro_zone` must carry `state == OnHex` with the local
/// `(q, r)` of the destination tile in its upper bits — the same
/// encoding `propose_action` uses for hex-rooted action coords.
///
/// Procedure:
/// 1. Resolve caller → `Player` → `soul_card_id` → soul `Card`.
/// 2. A* on the world-hex grid between the soul's current global
///    coord and the target's global coord, on a single surface.
///    Tile def 0 (empty) is impassable; every other tile resolves
///    its `cost` trait (see [`tile_cost`]).
/// 3. For each step in the path, write a future-stamped Card version
///    row that updates the soul's `(surface, macro_zone, micro_zone,
///    micro_location)` to the next tile. Step time is the cumulative
///    `0.5 * (from.cost + to.cost) / speed` rounded up to the next
///    whole second; `speed` (cost-units-per-second) comes from the
///    soul def's `speed` trait. Consecutive steps are forced strictly
///    ascending to avoid PK collisions on the `(card_id, time_secs)`
///    valid_at key.
///
/// **Interrupts.** A second `move_soul` call before the first path
/// completes is handled by
/// [`cards::scrub_or_repath_position_forward`] just below the
/// pathfind: pure-position queued steps (`position_dirty &&
/// !data_dirty`) get DELETED, data-bearing rows get their position
/// fields re-homed to the soul's `latest` row, and
/// `position_preserve` rows stop the walk (recipe pins / magnetic
/// anchors are author-pinned and movement can't yank them). Then
/// the new step writes proceed from `now` using `soul.latest` as
/// the path start — so the new path correctly resumes from whichever
/// hex the soul had reached on the server timeline at interrupt time.
#[reducer]
pub fn move_soul(
    ctx: &ReducerContext,
    target_surface: u8,
    target_macro_zone: u32,
    target_micro_zone: u8,
) -> Result<(), String> {
    let player_id = players::resolve_caller(ctx)?;
    let player = players::latest(ctx, player_id)
        .ok_or_else(|| format!("movement: player {player_id} not found"))?;
    if player.soul_card_id == 0 {
        return Err(format!(
            "movement: player {player_id} has no soul card"
        ));
    }
    let soul = cards::latest(ctx, player.soul_card_id).ok_or_else(|| {
        format!(
            "movement: soul card {} not found for player {player_id}",
            player.soul_card_id
        )
    })?;

    if soul.surface != target_surface {
        return Err(format!(
            "movement: cross-surface move not supported (soul on {}, target on {target_surface})",
            soul.surface
        ));
    }

    // Decode start (soul's current row).
    let (s_lq, s_lr, _) = unpack_micro_zone(soul.micro_zone);
    let (s_mq, s_mr) = unpack_macro_zone(soul.macro_zone);
    let start = Coord {
        q: s_mq as i32 * 8 + s_lq as i32,
        r: s_mr as i32 * 8 + s_lr as i32,
    };

    // Decode target.
    let (t_lq, t_lr, t_state) = unpack_micro_zone(target_micro_zone);
    if t_state != StackedState::OnHex {
        return Err(format!(
            "movement: target micro_zone state must be OnHex (got {t_state:?})"
        ));
    }
    let (t_mq, t_mr) = unpack_macro_zone(target_macro_zone);
    let goal = Coord {
        q: t_mq as i32 * 8 + t_lq as i32,
        r: t_mr as i32 * 8 + t_lr as i32,
    };

    if start == goal {
        return Ok(());
    }

    // Resolve this soul's movement speed from its definition's
    // `speed` trait, falling back to `DEFAULT_SOUL_SPEED` for any
    // soul-def that doesn't declare one. Used by `pathfind` and the
    // per-step write loop below — both feed the same value into
    // `step_cost` so the heuristic and the queued timings agree.
    let speed = soul_speed(soul.packed_definition);
    let path = pathfind(ctx, soul.surface, start, goal, speed)?;

    // Queue per-step soul-card writes at increasing future timestamps.
    let now = cards::now_ms(ctx);

    // Before queuing the new path, scrub or re-home any future rows
    // on the soul card. Pure-position queued rows from a prior
    // move_soul get deleted; rows carrying data (recipe completions,
    // expiry events, etc.) keep their data but have their position
    // fields re-homed to where the soul actually is *now* — without
    // this, a future flag-change row would still carry stale position
    // bytes from the old path. `position_preserve` rows (recipe slot
    // pins, magnetic anchors) STOP the walk: those positions are
    // author-pinned and movement can't yank them.
    //
    // Start position for re-homing is the soul's *current* state, not
    // the path's destination — movement will redo the steps from now,
    // re-stamping positions as it goes. Re-homing existing future
    // rows to `now`'s position keeps any intermediate data-row visible
    // at the soul's current spot until the new path's step writes
    // sweep past it.
    cards::scrub_or_repath_position_forward(
        ctx,
        soul.card_id,
        now,
        soul.surface,
        soul.macro_zone,
        soul.micro_zone,
        soul.micro_location,
    );

    // Queue one row per path step at increasing future timestamps.
    // The soul stays at its current row until the first step's
    // valid_at promotes — that natural delay is what creates the
    // "doesn't move, until it does" beat the client tweens between.
    let mut elapsed: f32 = 0.0;
    let mut last_time = now;
    for window in path.windows(2) {
        let from = window[0];
        let to = window[1];
        let from_cost = tile_def_at(ctx, soul.surface, from)
            .and_then(tile_cost)
            .ok_or_else(|| {
                format!(
                    "movement: step from ({}, {}) lands on impassable tile",
                    from.q, from.r
                )
            })?;
        let to_cost = tile_def_at(ctx, soul.surface, to)
            .and_then(tile_cost)
            .ok_or_else(|| {
                format!(
                    "movement: step to ({}, {}) lands on impassable tile",
                    to.q, to.r
                )
            })?;
        elapsed += step_cost(speed, from_cost, to_cost);
        // `elapsed` is real seconds (f32); convert to ms and round up
        // so sub-ms drift still advances the valid_at clock by at least
        // one ms, then clamp to strictly ascending so a burst of cheap
        // steps doesn't collide on the same ms.
        let mut step_time = now.saturating_add((elapsed * 1_000.0).ceil() as u64);
        if step_time <= last_time {
            step_time = last_time.saturating_add(1);
        }
        last_time = step_time;

        // Re-encode `to` into zone-relative form for the row write.
        let to_macro_q = to.q.div_euclid(8) as i16;
        let to_macro_r = to.r.div_euclid(8) as i16;
        let to_local_q = to.q.rem_euclid(8) as u8;
        let to_local_r = to.r.rem_euclid(8) as u8;
        let to_macro_zone = pack_macro_zone(to_macro_q, to_macro_r);
        let to_micro_zone = pack_micro_zone(to_local_q, to_local_r, StackedState::OnHex);

        cards::update_with_at(ctx, soul.card_id, step_time, |c| {
            c.surface = target_surface;
            c.macro_zone = to_macro_zone;
            c.micro_zone = to_micro_zone;
            // World-hex placements use `micro_location = 0` (no parent
            // card — the hex tile itself isn't a card). Mirrors the
            // world-rooted convention in `actions.rs` /
            // `world_gen.rs`.
            c.micro_location = 0;
            // Client render hint: this row participates in a planned
            // move. The arrow-overlay's target scan
            // (`DataManager.resolveCardTarget`) only picks up future
            // rows with BOTH `move_smooth` and `position_dirty` set
            // — `position_dirty` is auto-set by `write_at` because
            // the spatial fields above change row-over-row, but
            // `move_smooth` must be set explicitly here. Without it
            // the client can't tell a queued path step from any
            // other position write (teleport, push, etc.) and the
            // arrow never draws.
            c.flags |= FLAG_MOVE_SMOOTH;
        });
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
