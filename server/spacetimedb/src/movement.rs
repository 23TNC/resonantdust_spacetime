use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use spacetimedb::{reducer, ReducerContext};

use crate::cards;
use crate::packed::{
    pack_macro_zone, pack_micro_zone, tile_byte, unpack_macro_zone, unpack_micro_zone,
    valid_at_time, StackedState,
};
use crate::players;
use crate::zones::zones as _zones_table;

// ---- tuning ---------------------------------------------------------

/// Soul speed in seconds-per-cost-unit. The per-step cost formula is
/// `0.5 * SOUL_SPEED * (from.cost + to.cost)`, so with `SOUL_SPEED = 1`
/// and every tile costing `1` each hex step takes exactly 1 second
/// (the user's "1 tile per second" baseline).
///
/// Per-soul speed is the obvious next move — store on the soul card
/// (aspect, flag, or new column) and resolve in `move_soul`. Today
/// every soul shares this constant.
const SOUL_SPEED: f32 = 1.0;

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

/// Traversal cost of a tile, as a multiplier into the
/// `step_cost` formula. Today every populated tile is cost 1; def_id
/// 0 (empty / cleared tile) is impassable.
///
/// Future: drive from the tile card definition's aspects (e.g. an
/// `"cost"` aspect, defaulting to 1) so recipe authors can author
/// terrain difficulty in JSON. The signature already returns
/// `Option<f32>` so the impassable case stays clean once aspect
/// lookup lands.
fn tile_cost(def_id: u8) -> Option<f32> {
    if def_id == 0 {
        None
    } else {
        Some(1.0)
    }
}

/// Time (in seconds, before rounding to whole `valid_at` seconds) for
/// a single hex step from a tile of cost `from` to a tile of cost
/// `to`. Splits the cost equally between leaving the current tile
/// and entering the next, matching the user's spec
/// `0.5 * speed * (A.cost + B.cost)`.
fn step_cost(from: f32, to: f32) -> f32 {
    0.5 * SOUL_SPEED * (from + to)
}

/// A* over the world-hex grid. Returns the path inclusive of both
/// endpoints — `[start, …, goal]` — ordered by traversal.
fn pathfind(
    ctx: &ReducerContext,
    surface: u8,
    start: Coord,
    goal: Coord,
) -> Result<Vec<Coord>, String> {
    // Heuristic scaling: with `tile_cost = 1` everywhere, the minimum
    // possible step cost is `step_cost(1, 1) = SOUL_SPEED`. The
    // heuristic `hex_distance * SOUL_SPEED` is therefore admissible
    // (never overestimates). Bumping the minimum tile cost in the
    // future will require lowering this multiplier (or computing it
    // from the actual minimum) to keep admissibility.
    let h_scale = SOUL_SPEED;

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
            let tentative = current_g + step_cost(curr_cost, neigh_cost);
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
///    Tile def 0 (empty) is impassable; everything else has uniform
///    cost 1 today (placeholder for per-tile traversal costs).
/// 3. For each step in the path, write a future-stamped Card version
///    row that updates the soul's `(surface, macro_zone, micro_zone,
///    micro_location)` to the next tile. Step time is the cumulative
///    `0.5 * SOUL_SPEED * (from.cost + to.cost)` rounded up to the
///    next whole second; consecutive steps are forced strictly
///    ascending to avoid PK collisions on the `(card_id, time_secs)`
///    valid_at key.
///
/// **Limitation:** existing future-stamped soul rows are not cleared
/// before queuing. If `move_soul` is called twice in quick
/// succession the second path stacks on top of the first; once the
/// second path completes, any remaining rows from the first
/// (deeper-stamped) will resurrect and the soul will appear to
/// teleport back along the first path. The simple cleanup is to
/// delete every soul row at `valid_at_time > now` before queuing —
/// but recipe-driven moves (no such thing yet, but coming) would
/// also be wiped, so a `flags` bit distinguishing "movement" rows
/// is the right escape hatch. Punted until the second feature
/// actually exists.
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

    let path = pathfind(ctx, soul.surface, start, goal)?;

    // Queue per-step soul-card writes at increasing future timestamps.
    let now = (ctx.timestamp.to_micros_since_unix_epoch() / 1_000_000) as u32;
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
        elapsed += step_cost(from_cost, to_cost);
        // Round up so sub-second steps still advance the valid_at
        // clock by at least one second, then clamp to strictly
        // ascending so a path of cost-0.5 steps doesn't have two
        // writes collide on the same second.
        let mut step_time = now.saturating_add(elapsed.ceil() as u32);
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
        // Default tile cost 1 + speed 1 → 1 sec per step.
        assert!((step_cost(1.0, 1.0) - 1.0).abs() < f32::EPSILON);
        // Variable: 0.5 * 1 * (1 + 2) = 1.5
        assert!((step_cost(1.0, 2.0) - 1.5).abs() < f32::EPSILON);
        // Variable: 0.5 * 1 * (2 + 2) = 2
        assert!((step_cost(2.0, 2.0) - 2.0).abs() < f32::EPSILON);
    }
}
