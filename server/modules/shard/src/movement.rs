//! `move_soul` — write a soul's authoritative world position.
//!
//! **Reduced from the shard original.** The shard `move_soul` validated every
//! step of the path against the zone's per-tile traversability + movement cost.
//! Those tiles live in the `regions` database now, so a cards-side reducer can't
//! read them (no cross-DB access). Until `move_soul` moves to the gate pipeline
//! (gather the zone tiles → validate the path → apply), this trusts the client's
//! path and writes the destination position only. Soul position is otherwise
//! client-local; this is the state-changing sync write.

use spacetimedb::{reducer, ReducerContext, SpacetimeType};

use crate::cards;
use crate::packed::with_surface;

/// One step of a movement path — a tile address. Mirrors the shard wire type so
/// the client's `move_soul` call shape is unchanged.
#[derive(SpacetimeType, Debug, Clone, Copy)]
pub struct TilePoint {
    pub surface: u8,
    pub macro_zone: u64,
    pub micro_location: u32,
}

/// Move `soul_id` to the last tile of `path`. Validates ownership + that the
/// soul isn't held by an in-flight action; the per-tile traversability check is
/// dropped pending the gate-side zone gather (see module docs).
#[reducer]
pub fn move_soul(
    ctx: &ReducerContext,
    client_time_ms: u64,
    caller_player_id: u32,
    soul_id: u32,
    path: Vec<TilePoint>,
) -> Result<(), String> {
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;
    let soul = cards::prior_at(ctx, soul_id, now_ms)
        .ok_or_else(|| format!("movement: soul card {soul_id} not found"))?;
    if soul.owner_id != caller_player_id {
        return Err(format!(
            "movement: soul card {soul_id} is owned by player {} (not {caller_player_id})",
            soul.owner_id
        ));
    }
    if cards::slot_claim_count(soul.flags) > 0
        || cards::slot_borrow_count(soul.flags) > 0
        || cards::position_hold_count(soul.flags) > 0
    {
        return Err(format!(
            "movement: soul card {soul_id} is held by an in-flight action"
        ));
    }

    let dest = path
        .last()
        .ok_or_else(|| "movement: empty path".to_string())?;

    cards::update_with_at(ctx, soul_id, now_ms, |c| {
        c.macro_zone = with_surface(dest.macro_zone, dest.surface);
        c.micro_location = dest.micro_location;
    });
    Ok(())
}
