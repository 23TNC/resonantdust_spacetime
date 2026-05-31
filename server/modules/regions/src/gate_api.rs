//! Gate-facing write reducers for this `regions` shard. Authorization is the
//! gateway's job — these trust their arguments.
//!
//! The card-shard ref-count reducers (`acquire_card_shard` /
//! `release_card_shard`) live in `card_shards`; this adds the recipe tile
//! write. (`request_zone` / terrain generation already exist in `regions`.)

use resonantdust_content::card_model::{tile_stock, HoldField};
use spacetimedb::{reducer, ReducerContext};

use crate::cards;
use crate::zones;

/// Tile-stock op codes shared with the gateway's apply step.
mod stock_op {
    pub const SUB: u8 = 0;
    pub const ADD: u8 = 1;
    pub const SET: u8 = 2;
}

/// Hold-kind selector shared with the gateway. Matches `HoldField`'s
/// discriminants (`content::card_model`) and the cards module's `hold_kind`.
fn hold_field(kind: u8) -> Result<HoldField, String> {
    Ok(match kind {
        0 => HoldField::Touch,
        1 => HoldField::SlotHold,
        2 => HoldField::SlotShare,
        3 => HoldField::PositionHold,
        other => return Err(format!("tile hold: unknown kind {other}")),
    })
}

/// Promote the tile at hex `(q, r)` of `(surface, macro_zone)` into a tile-card
/// (idempotent — a no-op if one already exists there). The gate calls this at
/// action *acceptance* so the tile is a real card for the action's life; all
/// subsequent tile writes key on position, so the gate never needs the
/// (server-allocated) id.
#[reducer]
pub fn promote_tile(
    ctx: &ReducerContext,
    time_ms: u64,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
) -> Result<(), String> {
    cards::find_or_create_tile_card(ctx, surface, macro_zone, q, r, time_ms)?;
    Ok(())
}

/// Acquire hold `kind` on the tile at hex `(q, r)`, promoting it first. `kind`
/// comes from the recipe's slot verb (1=slot_hold for `use`/`claim`, 2=slot_share
/// for `share`/`borrow`, 0=touch, 3=position_hold). An exclusive slot_hold is the
/// concurrent-action guard: rejects (and the gate's apply aborts → `call_err`) if
/// the tile is already slot-held.
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn acquire_tile_hold(
    ctx: &ReducerContext,
    time_ms: u64,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    kind: u8,
) -> Result<(), String> {
    cards::acquire_tile_hold(ctx, surface, macro_zone, q, r, hold_field(kind)?, time_ms)?;
    Ok(())
}

/// Release one reference of hold `kind` on the tile at hex `(q, r)` (no-op if the
/// tile-card or hold is absent). The gate calls this at completion.
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn release_tile_hold(
    ctx: &ReducerContext,
    time_ms: u64,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    kind: u8,
) -> Result<(), String> {
    cards::release_tile_hold(ctx, surface, macro_zone, q, r, hold_field(kind)?, time_ms);
    Ok(())
}

/// Apply a recipe's tile-stock effect (`<tile>.aspect.X.{sub,add,set}`) to the
/// tile-card at hex `(q, r)`, mutating stock `slot` by `op`/`delta`. The gate
/// future-stamps this at completion; the tile is normally already promoted (by
/// `acquire_tile_hold` at acceptance), but this find-or-creates defensively.
/// `op`: 0=sub, 1=add, 2=set. Stock clamps to the 2-bit field (0..3).
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn set_tile_stock(
    ctx: &ReducerContext,
    time_ms: u64,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    slot: u8,
    op: u8,
    delta: u8,
) -> Result<(), String> {
    let tile = cards::find_or_create_tile_card(ctx, surface, macro_zone, q, r, time_ms)?;
    let current = tile_stock(tile.flags_bk, slot as usize);
    let next = match op {
        stock_op::SUB => current.saturating_sub(delta),
        stock_op::ADD => current.saturating_add(delta).min(0b11),
        stock_op::SET => delta.min(0b11),
        other => return Err(format!("set_tile_stock: unknown op {other}")),
    };
    cards::set_tile_stock(ctx, tile.card_id, time_ms, slot as usize, next);
    Ok(())
}

/// Set a single tile of `zone_id` at `(row, col)` to `def_id` with per-row
/// stocks, stamped at `time_ms`. The gateway resolves `zone_id` from its
/// gathered zone snapshot. A recipe tile effect (`<tile>.aspect.X.set`).
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn set_tile(
    ctx: &ReducerContext,
    zone_id: u32,
    time_ms: u64,
    row: u8,
    col: u8,
    def_id: u16,
    stock0: u8,
    stock1: u8,
) -> Result<(), String> {
    if zones::set_tile_at(ctx, zone_id, time_ms, row, col, def_id, stock0, stock1).is_none() {
        return Err(format!(
            "set_tile: zone {zone_id} not found or row/col out of range"
        ));
    }
    Ok(())
}
