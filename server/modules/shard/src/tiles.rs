//! Tile-cards — promote / stock / hold helpers over the canonical [`Card`]
//! table.
//!
//! A zone tile that participates in a recipe (or has cards stacked on it) is
//! **promoted** into a real card: `card_type = 7`, per-cell stock in the card's
//! `stock` byte, placed via the shared [`Micro`] model. Tile-cards are positional
//! and live in the region databases alongside the zones they shadow, so their
//! `card_id`s carry the `CARD_DB_REGIONS` database bit (stamped by
//! [`crate::cards::next_card_id`] reading this deployment's [`ShardIdentity`])
//! and the gateway routes reads/writes to the region DB.
//!
//! These are thin wrappers over the canonical bitemporal row ops in
//! [`crate::cards`] — the same `write_at`/`prior_at`/`update_with_at` the
//! owner-card path uses, so tile-cards get forward-propagation, the souls
//! write hook (a no-op for non-soul tiles), and the deferred-follower cascade
//! for free. This module is what replaced the regions shard's drifted partial
//! copy of those primitives.
//!
//! [`ShardIdentity`]: crate::cards::ShardIdentity

use resonantdust_codec::card_model::{stock, write_stock};
use spacetimedb::ReducerContext;

use crate::cards::cards;
use crate::cards::{create_at, next_card_id, prior_at, update_with_at, Card, Micro};
use crate::packed::{pack_definition, unpack_definition, unpack_zone_definition, with_surface};
use crate::zones;

/// `card_type` of the tile-as-card family (zone-tile cards). Mirrors the
/// constant the gateway/content use to recognise tiles.
pub const TILE_CARD_TYPE: u8 = 7;

/// Find the loose tile-card at hex `(q, r)` of `(surface, macro_zone)`, if one
/// has been promoted. A tile-card is a `TILE_CARD_TYPE` card placed loose
/// (snapped) at the cell.
pub fn find_tile_card_at(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    time_ms: u64,
) -> Option<Card> {
    let full_macro = with_surface(macro_zone, surface);
    let mut seen = std::collections::BTreeSet::new();
    for row in ctx.db.cards().macro_zone().filter(full_macro) {
        if !seen.insert(row.card_id) {
            continue;
        }
        let Some(card) = prior_at(ctx, row.card_id, time_ms) else {
            continue;
        };
        let (card_type, _) = unpack_definition(card.packed_definition);
        if card_type != TILE_CARD_TYPE {
            continue;
        }
        if let Micro::Loose { local_q, local_r, .. } = Micro::of(card.micro_location, card.flags) {
            if local_q == q && local_r == r {
                return Some(card);
            }
        }
    }
    None
}

/// Promote (or find) the tile-card at hex `(q, r)`. If a tile-card already
/// exists there it is returned unchanged (idempotent). Otherwise the zone tile
/// is read and a new tile-card is created carrying its def (`card_type=7`) and
/// stock, placed loose-snapped at the cell, owned by the zone's owner.
pub fn find_or_create_tile_card(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    time_ms: u64,
) -> Result<Card, String> {
    if let Some(card) = find_tile_card_at(ctx, surface, macro_zone, q, r, time_ms) {
        return Ok(card);
    }
    let full_macro = with_surface(macro_zone, surface);
    let zone = zones::latest_for(ctx, full_macro)
        .ok_or_else(|| format!("find_or_create_tile_card: no zone at {full_macro}"))?;
    let (def_id, stock0, stock1) = zone
        .tile_at(r, q)
        .ok_or_else(|| format!("find_or_create_tile_card: cell ({q},{r}) out of range"))?;
    if def_id == 0 {
        return Err(format!("find_or_create_tile_card: cell ({q},{r}) is empty"));
    }
    let tile_card_type = unpack_zone_definition(zone.packed_definition);
    let packed_def = pack_definition(tile_card_type, def_id);
    let stock_byte = write_stock(write_stock(0, 0, stock0), 1, stock1);
    let card_id = next_card_id(ctx);
    let card = create_at(
        ctx,
        card_id,
        time_ms,
        full_macro,
        Micro::snap(q, r),
        zone.owner_id,
        packed_def,
        /* flags */ 0,
        stock_byte,
    );
    Ok(card)
}

/// Set tile-card per-row stock `slot` to `value` at `time_ms`.
pub fn set_tile_stock(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    slot: usize,
    value: u8,
) -> Option<Card> {
    update_with_at(ctx, card_id, time_ms, |c| {
        c.stock = write_stock(c.stock, slot, value);
    })
}

/// Card-priority tile read: a promoted tile-card's `(packed_def, stock0, stock1)`
/// if one exists at the cell, else the zone slot's. `None` if no zone / empty.
pub fn tile_full_view(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    time_ms: u64,
) -> Option<(u16, u8, u8)> {
    if let Some(card) = find_tile_card_at(ctx, surface, macro_zone, q, r, time_ms) {
        return Some((
            card.packed_definition,
            stock(card.stock, 0),
            stock(card.stock, 1),
        ));
    }
    let zone = zones::latest_for(ctx, with_surface(macro_zone, surface))?;
    let (def_id, stock0, stock1) = zone.tile_at(r, q)?;
    if def_id == 0 {
        return None;
    }
    Some((pack_definition(unpack_zone_definition(zone.packed_definition), def_id), stock0, stock1))
}
