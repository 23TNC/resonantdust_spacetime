//! `move_soul` — stamp a soul's authoritative world position ONE tile ahead, at
//! the time it will ARRIVE there.
//!
//! Single-step, future-stamped. The client requests one tile at a time, passing
//! the destination cell + the `arrival_ms` it computed from the tile costs and
//! the soul's `speed` (`travel = (cost_cur + cost_dst)·1000 / (2·speed)`). We
//! write the destination row at `arrival_ms` (a FUTURE `valid_at`): the soul does
//! NOT exist at the new tile until the clock reaches it — it stays at its current
//! cell, and the client interpolates the visual position between the two rows'
//! `valid_at`s. The client pipelines: it requests the next step the moment it
//! receives this future row, giving the server the whole traversal to validate
//! the next one.
//!
//! Content validation (recompute the cost-based `arrival_ms`, adjacency,
//! traversability) is the GATE's job — it has the zone tiles + the soul's
//! `speed`; this content-agnostic shard reducer trusts the gate-validated
//! `arrival_ms` and only sanity-bounds it. Soul position is otherwise
//! client-local; this is the state-changing sync write.

use spacetimedb::{reducer, ReducerContext, SpacetimeType};

use crate::cards;
use crate::packed::{surface_of, unpack_macro_zone, with_surface, WORLD_LAYER};

/// A tile address — the destination cell of one movement step. (Name kept from
/// the path era; a step is a single point now.)
#[derive(SpacetimeType, Debug, Clone, Copy)]
pub struct TilePoint {
    pub surface: u8,
    pub macro_zone: u64,
    pub micro_location: u32,
}

/// The widest a single-tile traversal may take — a sanity bound on
/// `arrival_ms - depart_ms` (the slowest tile/​speed combo is a few seconds).
const MAX_STEP_MS: u64 = 60_000;

/// The world hex `(q, r)` of a loose world card from its `(macro_zone,
/// micro_location, flags)` — `None` if it's not loose on the world surface.
fn world_cell(macro_zone: u64, micro_location: u32, flags: u32) -> Option<(i32, i32)> {
    if surface_of(macro_zone) != WORLD_LAYER {
        return None;
    }
    use crate::packed::world_tile;
    let (zq, zr) = unpack_macro_zone(macro_zone);
    match cards::Micro::of(micro_location, flags) {
        cards::Micro::Loose { local_q, local_r, .. } => {
            Some((world_tile(zq, local_q), world_tile(zr, local_r)))
        }
        cards::Micro::Stacked { .. } => None,
    }
}

/// Axial-hex adjacency (the 6 neighbors).
fn hex_adjacent(a: (i32, i32), b: (i32, i32)) -> bool {
    matches!(
        (b.0 - a.0, b.1 - a.1),
        (1, 0) | (-1, 0) | (0, 1) | (0, -1) | (1, -1) | (-1, 1)
    )
}

/// Stamp `soul_id`'s arrival at the adjacent tile `dest` at the future `valid_at`
/// `arrival_ms`. The GATE re-derives `arrival_ms` from content (the `from`/`dest`
/// tile `cost`s + the soul's `speed`) and overrides the client's; THIS reducer
/// verifies the gate's content inputs against the authoritative card state — so a
/// spoofed `soul_def`/`from` just fails. Checks: ownership, not-held, the claimed
/// `soul_def` matches the soul's real def, the soul's actual cell AT `depart_ms`
/// (its future-inclusive position when this step departs) equals `(from_q,
/// from_r)`, `dest` is hex-adjacent to it, and the timing is a sane future window.
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn move_soul(
    ctx: &ReducerContext,
    client_time_ms: u64,
    caller_player_id: u32,
    soul_id: u32,
    soul_def: u16,
    from_q: i32,
    from_r: i32,
    dest: TilePoint,
    depart_ms: u64,
    arrival_ms: u64,
) -> Result<(), String> {
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;
    let soul = cards::prior_at(ctx, soul_id, now_ms)
        .ok_or_else(|| format!("movement: soul card {soul_id} not found"))?;
    // A world soul's `owner_id` is its player_soul CARD, not the player — walk the
    // owner chain to the player and check the caller owns it.
    let soul_player = cards::owning_player(ctx, soul_id)
        .ok_or_else(|| format!("movement: can't resolve owning player of soul {soul_id}"))?;
    if soul_player != caller_player_id {
        return Err(format!(
            "movement: soul card {soul_id} is owned by player {soul_player} (not {caller_player_id})"
        ));
    }
    if cards::slot_claim_count(soul.flags) > 0
        || cards::slot_borrow_count(soul.flags) > 0
        || cards::position_hold_count(soul.flags) > 0
    {
        return Err(format!("movement: soul card {soul_id} is held by an in-flight action"));
    }
    // Anti-spoof: the gate computed travel from THIS `soul_def`; it must be real
    // (else a client could claim a faster def to shave the cost-derived arrival).
    if soul.packed_definition != soul_def {
        return Err(format!(
            "movement: soul {soul_id} def {} ≠ claimed {soul_def}",
            soul.packed_definition
        ));
    }
    // Position authority: the soul's cell AT `depart_ms` (future-inclusive — a
    // pipelined step departs from the prior step's already-stamped row) must be the
    // `from` the gate priced the move from.
    let depart_row = cards::prior_at(ctx, soul_id, depart_ms)
        .ok_or_else(|| format!("movement: no soul row at depart {depart_ms}"))?;
    let actual_from = world_cell(depart_row.macro_zone, depart_row.micro_location, depart_row.flags)
        .ok_or_else(|| format!("movement: soul {soul_id} not loose on the world at depart"))?;
    if actual_from != (from_q, from_r) {
        return Err(format!(
            "movement: soul {soul_id} is at {actual_from:?} at depart, not the claimed ({from_q},{from_r})"
        ));
    }
    let dest_cell = world_cell(with_surface(dest.macro_zone, dest.surface), dest.micro_location, 0)
        .ok_or_else(|| "movement: dest is not a loose world cell".to_string())?;
    if !hex_adjacent((from_q, from_r), dest_cell) {
        return Err(format!(
            "movement: dest {dest_cell:?} is not adjacent to from ({from_q},{from_r})"
        ));
    }
    if arrival_ms <= depart_ms {
        return Err(format!("movement: arrival_ms {arrival_ms} not after depart_ms {depart_ms}"));
    }
    if arrival_ms - depart_ms > MAX_STEP_MS {
        return Err(format!(
            "movement: step {} ms exceeds the {MAX_STEP_MS} ms bound",
            arrival_ms - depart_ms
        ));
    }
    if depart_ms + MAX_STEP_MS < now_ms {
        return Err(format!("movement: depart_ms {depart_ms} is stale (now {now_ms})"));
    }

    // Future-stamp the destination cell at the arrival time: the prior (current)
    // row stays valid until then, so the soul "arrives" exactly when the clock
    // reaches `arrival_ms` (the client's `current(now)` excludes it until then).
    cards::update_with_at(ctx, soul_id, arrival_ms, |c| {
        c.macro_zone = with_surface(dest.macro_zone, dest.surface);
        c.micro_location = dest.micro_location;
    });
    Ok(())
}
