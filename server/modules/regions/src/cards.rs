//! Tile-cards — the regions-local `Card` table.
//!
//! A zone tile that participates in a recipe (or has cards stacked on it) is
//! **promoted** into a real card here: `card_type = 7`, per-cell stock packed
//! into `flags_bk`, placed via the shared [`Micro`] model. Because tile-cards
//! are positional, they live with the zones they shadow (this DB), so the
//! tile↔zone reconciliation (promote here, demote in GC) stays intra-`regions`.
//! Their `card_id`s carry the `regions` database bit (`pack_card_id(CARD_DB_REGIONS, …)`)
//! so the gateway routes reads/writes here, not to the owner-sharded `cards` DB.
//!
//! This is a deliberately small slice of the `cards` module's machinery: the
//! bitemporal row ops + the tile promote/stock helpers. Holds and the GC
//! demotion sweep land next (see `gc.rs`). The placement / stock bit math is the
//! shared `content::card_model`, so this never diverges from `cards`.

use resonantdust_data::card_model::{
    decrement_hold, hold_count, increment_hold, tile_stock, write_tile_stock, HoldField, Micro,
};
use crate::packed::{
    card_local_of, pack_card_id, pack_definition, unpack_definition, unpack_zone_definition,
    with_surface, CARD_DB_REGIONS, CARD_LOCAL_MASK, SNAP_HEX,
};
use spacetimedb::{table, ReducerContext, Table};

use crate::packed::{pack_valid_at, valid_at_time};
use crate::sequence;
use crate::zones;

/// `card_type` of the tile-as-card family (zone-tile cards). Mirrors the
/// constant the gateway/content use to recognise tiles.
pub const TILE_CARD_TYPE: u8 = 7;

/// First non-reserved local id (mirrors `cards::FIRST_CARD_ID`); locals below
/// stay sentinel-reserved so a tile-card id never collides with the WORLD `0`.
const FIRST_LOCAL: u32 = 1024;

/// The `regions` shard these tile-cards belong to (single shard today). Folded
/// into every tile-card id alongside the `CARD_DB_REGIONS` database bit.
const REGIONS_SHARD: u16 = 0;

/// A tile-card row — schema-compatible with `cards::Card` so the gateway's
/// `CardView` reads it uniformly. History-style (one row per version).
#[table(accessor = cards, public)]
pub struct Card {
    #[primary_key]
    pub valid_at: u64,
    #[index(btree)]
    pub card_id: u32,
    #[index(btree)]
    pub macro_zone: u64,
    #[index(btree)]
    pub micro_location: u32,
    #[index(btree)]
    pub owner_id: u32,
    pub packed_definition: u16,
    pub flags_state: u32,
    pub flags_bk: u32,
}

/// One-row local-id allocator for tile-cards (mirrors `cards::CardIdCounter`).
#[table(accessor = card_id_counter)]
pub struct CardIdCounter {
    #[primary_key]
    pub id: u8,
    pub next: u32,
}

/// Allocate the next tile-card id: a fresh per-shard local, tagged with the
/// `regions` database bit + this shard. `card_db_of(id) == CARD_DB_REGIONS`, so
/// the gateway routes it here.
pub fn next_card_id(ctx: &ReducerContext) -> u32 {
    let next_local = match ctx.db.card_id_counter().id().find(0) {
        Some(counter) => {
            ctx.db.card_id_counter().id().delete(0);
            counter.next
        }
        None => ctx
            .db
            .cards()
            .iter()
            .map(|c| card_local_of(c.card_id))
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(FIRST_LOCAL),
    };
    ctx.db.card_id_counter().insert(CardIdCounter {
        id: 0,
        next: next_local.saturating_add(1).min(CARD_LOCAL_MASK),
    });
    pack_card_id(CARD_DB_REGIONS, REGIONS_SHARD, next_local)
}

// ---- bitemporal row ops (mirror cards::{prior_at, latest, write_at}) -----

/// Latest version of `card_id` at or before `time_ms`, or `None`.
pub fn prior_at(ctx: &ReducerContext, card_id: u32, time_ms: u64) -> Option<Card> {
    ctx.db
        .cards()
        .card_id()
        .filter(card_id)
        .filter(|c| valid_at_time(c.valid_at) <= time_ms)
        .max_by_key(|c| valid_at_time(c.valid_at))
}

/// Write a tile-card version stamped at `time_ms` ("last write at this
/// `(card_id, time_ms)` wins" — same-ms rows are deleted first). No flag-diff
/// forward-propagation yet (holds land in P1d); tile stock changes are
/// future-stamped point writes, which this handles.
pub fn write_at(ctx: &ReducerContext, mut card: Card, time_ms: u64) -> Card {
    let card_id = card.card_id;
    let stale: Vec<u64> = ctx
        .db
        .cards()
        .card_id()
        .filter(card_id)
        .filter(|c| valid_at_time(c.valid_at) == time_ms)
        .map(|c| c.valid_at)
        .collect();
    for v in stale {
        ctx.db.cards().valid_at().delete(v);
    }
    card.valid_at = pack_valid_at(time_ms, sequence::next_sequence(ctx));
    ctx.db.cards().insert(card)
}

/// Create a new tile-card row at `time_ms`.
#[allow(clippy::too_many_arguments)]
pub fn create_at(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    macro_zone: u64,
    micro: Micro,
    owner_id: u32,
    packed_definition: u16,
    flags_state: u32,
    flags_bk: u32,
) -> Card {
    let (micro_location, flags_bk) = micro.apply(flags_bk);
    write_at(
        ctx,
        Card {
            valid_at: 0,
            card_id,
            macro_zone,
            micro_location,
            owner_id,
            packed_definition,
            flags_state,
            flags_bk,
        },
        time_ms,
    )
}

/// Read-modify-write the latest version at `time_ms` through `f`, returning the
/// new row (or `None` if the card doesn't exist).
pub fn update_with_at<F>(ctx: &ReducerContext, card_id: u32, time_ms: u64, f: F) -> Option<Card>
where
    F: FnOnce(&mut Card),
{
    let mut c = prior_at(ctx, card_id, time_ms)?;
    f(&mut c);
    Some(write_at(ctx, c, time_ms))
}

// ---- tile promote / stock (mirror shard::cards tile helpers) -------------

/// Find the loose tile-card at hex `(q, r)` of `(surface, macro_zone)`, if one
/// has been promoted. A tile-card is a `TILE_CARD_TYPE` card placed loose
/// (snapped) at the cell. (Rect-stacked tiles are a later refinement.)
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
        if let Micro::Loose { local_q, local_r, .. } = Micro::of(card.micro_location, card.flags_bk) {
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
    let mut bk = 0u32;
    bk = write_tile_stock(bk, 0, stock0);
    bk = write_tile_stock(bk, 1, stock1);
    let card_id = next_card_id(ctx);
    let card = create_at(
        ctx,
        card_id,
        time_ms,
        full_macro,
        Micro::snap(q, r, SNAP_HEX),
        zone.owner_id,
        packed_def,
        /* flags_state */ 0,
        bk,
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
        c.flags_bk = write_tile_stock(c.flags_bk, slot, value);
    })
}

// ---- tile-card holds (promote-up-front, the concurrent-action guard) -----

/// Acquire one reference of hold `field` on the tile-card at hex `(q, r)`,
/// **promoting it first** (idempotent `find_or_create`). A promoted, held tile is
/// just a card in the hex branch — the recipe's slot verb picks the `field`
/// (`use`/`claim`→`SlotHold`, `share`/`borrow`→`SlotShare`), exactly like any
/// bound card.
///
/// For an exclusive `SlotHold` this is the **concurrent-cut guard**: reducers are
/// DB-serialized, so a second action's acquire reads this one's committed hold
/// (same-`time_ms` rows coalesce in [`write_at`]) and is rejected here. Returns
/// the new tile-card row.
pub fn acquire_tile_hold(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    field: HoldField,
    time_ms: u64,
) -> Result<Card, String> {
    let tile = find_or_create_tile_card(ctx, surface, macro_zone, q, r, time_ms)?;
    if field == HoldField::SlotHold && hold_count(tile.flags_bk, HoldField::SlotHold) > 0 {
        return Err(format!(
            "acquire_tile_hold: tile ({q},{r}) of zone {macro_zone} is already exclusively held"
        ));
    }
    update_with_at(ctx, tile.card_id, time_ms, |c| {
        c.flags_bk = increment_hold(c.flags_bk, field);
    })
    .ok_or_else(|| format!("acquire_tile_hold: tile-card {} vanished", tile.card_id))
}

/// Release one reference of hold `field` on the tile-card at hex `(q, r)`, if one
/// exists (no-op otherwise). Mirror of [`acquire_tile_hold`]; once a tile-card is
/// hold-free and clean, the GC demotion sweep folds it back into the zone.
pub fn release_tile_hold(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    field: HoldField,
    time_ms: u64,
) -> Option<Card> {
    let tile = find_tile_card_at(ctx, surface, macro_zone, q, r, time_ms)?;
    update_with_at(ctx, tile.card_id, time_ms, |c| {
        c.flags_bk = decrement_hold(c.flags_bk, field);
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
            tile_stock(card.flags_bk, 0),
            tile_stock(card.flags_bk, 1),
        ));
    }
    let zone = zones::latest_for(ctx, with_surface(macro_zone, surface))?;
    let (def_id, stock0, stock1) = zone.tile_at(r, q)?;
    if def_id == 0 {
        return None;
    }
    Some((pack_definition(unpack_zone_definition(zone.packed_definition), def_id), stock0, stock1))
}
