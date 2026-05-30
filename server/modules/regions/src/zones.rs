use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{table, ReducerContext, Table};

use crate::packed::{self, pack_valid_at, pack_zone_definition, valid_at_time};
use crate::sequence;

#[table(accessor = zones, public)]
pub struct Zone {
    #[primary_key]
    pub valid_at: u64,
    #[index(btree)]
    pub zone_id: u32,
    /// Packed location key `[reserved:u32 | surface:u8 | payload:u24]`.
    /// `surface` lives at bits 24-31 (read via `packed::surface_of`); there
    /// is no separate surface column. World payload = `(zone_q, zone_r)`,
    /// non-world payload = container id.
    #[index(btree)]
    pub macro_zone: u64,
    pub packed_definition: u8,
    /// Container id of every synthetic hex derived from this
    /// zone's tile bytes. Usually a `card_id` (consistent with
    /// `Card.owner_id` semantics when `FLAG_OWNED_BY_PLAYER` is
    /// clear). `0` is the WORLD sentinel — world zones use it, and
    /// `ProductOwner::Hex` resolution falls back to the caller's
    /// soul when this is `0`. Mini_zones store `anchor.card_id`
    /// here.
    #[index(btree)]
    pub owner_id: u32,
    // 64 tiles × u16 slot = 1024 bits = 16 u64. Each slot packs
    // `[def_id:u12 | stock0:u2 | stock1:u2]`. Use the helpers below
    // (`tile_at`, `tile_stock`, `assign_tile`, `assign_tile_stock`)
    // rather than touching the u64 fields directly. See
    // docs/TILE_ASPECTS.md.
    pub t0: u64,  pub t1: u64,  pub t2: u64,  pub t3: u64,
    pub t4: u64,  pub t5: u64,  pub t6: u64,  pub t7: u64,
    pub t8: u64,  pub t9: u64,  pub t10: u64, pub t11: u64,
    pub t12: u64, pub t13: u64, pub t14: u64, pub t15: u64,
}

impl Zone {
    /// Collect the 16 u64 tile fields into the fixed-size array the
    /// `packed::tile_*` helpers operate on.
    pub fn tiles(&self) -> [u64; packed::ZONE_TILE_U64_COUNT] {
        [
            self.t0,  self.t1,  self.t2,  self.t3,
            self.t4,  self.t5,  self.t6,  self.t7,
            self.t8,  self.t9,  self.t10, self.t11,
            self.t12, self.t13, self.t14, self.t15,
        ]
    }

    /// Write the 16-u64 tile array back into the struct fields.
    pub fn set_tiles(&mut self, tiles: &[u64; packed::ZONE_TILE_U64_COUNT]) {
        self.t0  = tiles[0];  self.t1  = tiles[1];  self.t2  = tiles[2];  self.t3  = tiles[3];
        self.t4  = tiles[4];  self.t5  = tiles[5];  self.t6  = tiles[6];  self.t7  = tiles[7];
        self.t8  = tiles[8];  self.t9  = tiles[9];  self.t10 = tiles[10]; self.t11 = tiles[11];
        self.t12 = tiles[12]; self.t13 = tiles[13]; self.t14 = tiles[14]; self.t15 = tiles[15];
    }

    /// Decode one row (0..=7) — returns `(def_id, stock0, stock1)`
    /// per column. `None` for out-of-range row.
    pub fn tile_row(&self, row: u8) -> Option<[(u16, u8, u8); 8]> {
        if row >= 8 { return None; }
        Some(packed::tile_row(&self.tiles(), row as usize))
    }

    /// Read a single tile by `(row, col)`. Returns
    /// `(def_id, stock0, stock1)`; `None` if either coord out of
    /// range.
    pub fn tile_at(&self, row: u8, col: u8) -> Option<(u16, u8, u8)> {
        if row >= 8 || col >= 8 { return None; }
        Some(packed::tile_full(&self.tiles(), (row * 8 + col) as usize))
    }

    /// Read just the def_id at `(row, col)`. Convenience for
    /// callers that don't need stock.
    pub fn tile_def_id_at(&self, row: u8, col: u8) -> Option<u16> {
        if row >= 8 || col >= 8 { return None; }
        Some(packed::tile_def_id(&self.tiles(), (row * 8 + col) as usize))
    }

    /// Read one stock slot (0 or 1) at `(row, col)`. Returns
    /// `None` for out-of-range coords or slot index.
    pub fn tile_stock_at(&self, row: u8, col: u8, slot: usize) -> Option<u8> {
        if row >= 8 || col >= 8 || slot >= packed::ZONE_TILE_STOCK_SLOTS {
            return None;
        }
        Some(packed::tile_stock(&self.tiles(), (row * 8 + col) as usize, slot))
    }

    /// Write a single tile by `(row, col)`: def_id + both stock
    /// slots in one call. Returns `false` for out-of-range coords.
    pub fn assign_tile(
        &mut self,
        row: u8,
        col: u8,
        def_id: u16,
        stock0: u8,
        stock1: u8,
    ) -> bool {
        if row >= 8 || col >= 8 { return false; }
        let mut packed = self.tiles();
        packed::set_tile_full(
            &mut packed,
            (row * 8 + col) as usize,
            def_id,
            stock0,
            stock1,
        );
        self.set_tiles(&packed);
        true
    }

    /// Write a single tile's def_id at `(row, col)` while
    /// preserving its stock slots.
    pub fn assign_tile_def(&mut self, row: u8, col: u8, def_id: u16) -> bool {
        if row >= 8 || col >= 8 { return false; }
        let mut packed = self.tiles();
        let idx = (row * 8 + col) as usize;
        let (_, s0, s1) = packed::tile_full(&packed, idx);
        packed::set_tile_full(&mut packed, idx, def_id, s0, s1);
        self.set_tiles(&packed);
        true
    }

    /// Write one stock slot at `(row, col)`. Other fields on the
    /// tile (def_id + the other stock) are left untouched.
    pub fn assign_tile_stock(
        &mut self,
        row: u8,
        col: u8,
        slot: usize,
        value: u8,
    ) -> bool {
        if row >= 8 || col >= 8 || slot >= packed::ZONE_TILE_STOCK_SLOTS {
            return false;
        }
        let mut packed = self.tiles();
        packed::set_tile_stock(&mut packed, (row * 8 + col) as usize, slot, value);
        self.set_tiles(&packed);
        true
    }
}

fn now_ms(ctx: &ReducerContext) -> u64 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64
}

// Latest row for a zone_id is the row with the largest time component of valid_at.
pub fn latest(ctx: &ReducerContext, zone_id: u32) -> Option<Zone> {
    ctx.db
        .zones()
        .zone_id()
        .filter(zone_id)
        .max_by_key(|z| valid_at_time(z.valid_at))
}

/// Latest row for the zone keyed by `macro_zone`. `macro_zone` now encodes
/// the surface band in bits 24-31 (world chunks at `surface=WORLD_LAYER`,
/// mini_zones at `surface=MINI_ZONE_LAYER` over the anchor id, etc.), so the
/// single packed value identifies the container exactly — no separate
/// surface filter. Returns `None` if no Zone exists at that address.
pub fn latest_for(ctx: &ReducerContext, macro_zone: u64) -> Option<Zone> {
    ctx.db
        .zones()
        .macro_zone()
        .filter(macro_zone)
        .max_by_key(|z| valid_at_time(z.valid_at))
}

/// Allocate a fresh `zone_id`. Scans the table for `max(zone_id) + 1`
/// at the time of the call — O(N) over the version-row count for
/// the zones table. Fine while the table is small; promote to a
/// counter-row pattern (mirroring `cards::next_card_id`) if/when
/// zone creation becomes a hot path.
pub fn next_zone_id(ctx: &ReducerContext) -> u32 {
    ctx.db
        .zones()
        .iter()
        .map(|z| z.zone_id)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

// Stamp valid_at = (zone_id, now) and write. If a row already exists at that
// exact key (two writes in the same second), the existing one is replaced —
// "always accept the most recent write".
fn write(ctx: &ReducerContext, zone: Zone) -> Zone {
    write_at(ctx, zone, now_ms(ctx))
}

// Like `write`, but stamps `valid_at` with a caller-supplied millisecond
// timestamp instead of `now`. Used by the action-completion path to apply
// zone changes (tile-byte clears, location-output writes) at the action's
// `completion_ms` rather than at the wall-clock moment the reducer runs —
// so the client doesn't see a tile change the instant an action starts, only
// when its buffered clock reaches the action's completion.
fn write_at(ctx: &ReducerContext, mut zone: Zone, time_ms: u64) -> Zone {
    // "Last write at this (zone_id, time_ms) wins." See
    // `cards::write_at` for the full rationale — same-time writes
    // would otherwise accumulate distinct rows under the new
    // sequence-bearing PK.
    let stale: Vec<u64> = ctx
        .db
        .zones()
        .zone_id()
        .filter(zone.zone_id)
        .filter(|z| valid_at_time(z.valid_at) == time_ms)
        .map(|z| z.valid_at)
        .collect();
    for v in stale {
        ctx.db.zones().valid_at().delete(v);
    }
    zone.valid_at = pack_valid_at(time_ms, sequence::next_sequence(ctx));
    ctx.db.zones().insert(zone)
}

// Insert a brand-new zone. valid_at is computed; pass 0 will be overwritten.
#[allow(clippy::too_many_arguments)]
pub fn create(
    ctx: &ReducerContext,
    zone_id: u32,
    macro_zone: u64,
    packed_definition: u8,
    owner_id: u32,
    tiles: [u64; packed::ZONE_TILE_U64_COUNT],
) -> Zone {
    create_at(ctx, zone_id, macro_zone, packed_definition, owner_id, tiles, now_ms(ctx))
}

// Insert a brand-new zone stamped at `time_ms`. Mirrors
// `cards::create_at` — use this from reducers that have resolved an
// `effective_now_ms` value so the row's `valid_at` matches the
// client's view of "now".
#[allow(clippy::too_many_arguments)]
pub fn create_at(
    ctx: &ReducerContext,
    zone_id: u32,
    macro_zone: u64,
    packed_definition: u8,
    owner_id: u32,
    tiles: [u64; packed::ZONE_TILE_U64_COUNT],
    time_ms: u64,
) -> Zone {
    write_at(
        ctx,
        Zone {
            valid_at: 0,
            zone_id,
            macro_zone,
            packed_definition,
            owner_id,
            t0:  tiles[0],  t1:  tiles[1],  t2:  tiles[2],  t3:  tiles[3],
            t4:  tiles[4],  t5:  tiles[5],  t6:  tiles[6],  t7:  tiles[7],
            t8:  tiles[8],  t9:  tiles[9],  t10: tiles[10], t11: tiles[11],
            t12: tiles[12], t13: tiles[13], t14: tiles[14], t15: tiles[15],
        },
        time_ms,
    )
}

// Pick up the latest row for `zone_id`, mutate it via `f`, write it back.
// Returns None if no prior row exists.
pub fn update_with<F>(ctx: &ReducerContext, zone_id: u32, f: F) -> Option<Zone>
where
    F: FnOnce(&mut Zone),
{
    let mut z = latest(ctx, zone_id)?;
    f(&mut z);
    Some(write(ctx, z))
}

// Like `update_with`, but stamps the resulting row at a specific
// `time_ms` rather than `now`. Used by the action-completion path to
// future-stamp tile writes at `completion_ms`.
pub fn update_with_at<F>(
    ctx: &ReducerContext,
    zone_id: u32,
    time_ms: u64,
    f: F,
) -> Option<Zone>
where
    F: FnOnce(&mut Zone),
{
    let mut z = latest(ctx, zone_id)?;
    f(&mut z);
    Some(write_at(ctx, z, time_ms))
}

// ---- single-field setters ---------------------------------------------

pub fn set_macro_zone(ctx: &ReducerContext, zone_id: u32, macro_zone: u64) -> Option<Zone> {
    update_with(ctx, zone_id, |z| z.macro_zone = macro_zone)
}

pub fn set_packed_definition(
    ctx: &ReducerContext,
    zone_id: u32,
    packed_definition: u8,
) -> Option<Zone> {
    update_with(ctx, zone_id, |z| z.packed_definition = packed_definition)
}

pub fn set_owner_id(ctx: &ReducerContext, zone_id: u32, owner_id: u32) -> Option<Zone> {
    update_with(ctx, zone_id, |z| z.owner_id = owner_id)
}

// Replace all 8 tiles in row `row` (0..8). Each entry is
// `(def_id, stock0, stock1)`.
pub fn set_tile_row(
    ctx: &ReducerContext,
    zone_id: u32,
    row: u8,
    tiles: [(u16, u8, u8); 8],
) -> Option<Zone> {
    if row >= 8 {
        return None;
    }
    update_with(ctx, zone_id, |z| {
        for (col, &(def_id, s0, s1)) in tiles.iter().enumerate() {
            z.assign_tile(row, col as u8, def_id, s0, s1);
        }
    })
}

// Replace all 64 tiles at once.
pub fn set_tile_rows(ctx: &ReducerContext, zone_id: u32, tiles: [u64; packed::ZONE_TILE_U64_COUNT]) -> Option<Zone> {
    update_with(ctx, zone_id, |z| {
        z.set_tiles(&tiles);
    })
}

// Replace one tile at `(row, col)` — def + both stocks. Stamps the
// new version row at *now*. Delegates to `set_tile_at` so both the
// prior-row read and the forward-propagation apply uniformly.
pub fn set_tile(
    ctx: &ReducerContext,
    zone_id: u32,
    row: u8,
    col: u8,
    def_id: u16,
    stock0: u8,
    stock1: u8,
) -> Option<Zone> {
    set_tile_at(ctx, zone_id, now_ms(ctx), row, col, def_id, stock0, stock1)
}

/// Write a single tile at `(row, col)` — def + both stocks —
/// stamping the new Zone version row at `time_ms`.
///
/// Two pieces of bookkeeping beyond the obvious row write, both
/// necessary for sane behaviour when actions on the same zone interleave
/// in time (likely once players act on different tiles of an 8×8 zone
/// in parallel):
///
/// 1. **Read from the prior row, not the latest.** The new row's
///    baseline is the row with the largest `valid_at_time ≤ time_ms`
///    — i.e., the state of the zone *just before* our write takes
///    effect. Reading `latest()` instead would pull in changes from
///    future-stamped rows that the client hasn't yet promoted to,
///    contaminating our row with not-yet-due changes.
///
/// 2. **Forward-propagate the change.** After writing our row, walk
///    every future row (`valid_at_time > time_ms`) in ascending
///    order. For each, if its full tile slot at `(row, col)` (def +
///    stocks together) still equals the prior row's value (i.e., the
///    tile was inherited from before our write — nobody deliberately
///    changed any field of it after us), overwrite it. Stop at the
///    first row where the slot already differs — that's a deliberate
///    change made by another action; clobbering it would corrupt the
///    other action's outcome.
///
/// The "stop on first deliberate change" rule keeps the propagation
/// safe in the presence of multiple actions on the same tile across
/// different times. The comparison covers the full u16 slot so a
/// stock-only mutation registers as a deliberate change too — see
/// `set_tile_stock_at` for stock-only writes that share this
/// machinery.
pub fn set_tile_at(
    ctx: &ReducerContext,
    zone_id: u32,
    time_ms: u64,
    row: u8,
    col: u8,
    def_id: u16,
    stock0: u8,
    stock1: u8,
) -> Option<Zone> {
    if row >= 8 || col >= 8 {
        return None;
    }

    // (1) Read the prior row: max valid_at_time ≤ time_ms.
    let mut prior = ctx
        .db
        .zones()
        .zone_id()
        .filter(zone_id)
        .filter(|z| valid_at_time(z.valid_at) <= time_ms)
        .max_by_key(|z| valid_at_time(z.valid_at))?;
    let old_slot = prior.tile_at(row, col).unwrap_or((0, 0, 0));
    let new_slot = (def_id & 0x0FFF, stock0 & 0x3, stock1 & 0x3);

    // If the prior row already has this slot, our write is a no-op.
    if old_slot == new_slot {
        return Some(prior);
    }

    prior.assign_tile(row, col, def_id, stock0, stock1);
    let written = write_at(ctx, prior, time_ms);

    // (2) Forward-propagate. Collect the future-row valid_ats first so
    // we're not holding an iterator across mutations of the same table.
    let mut future: Vec<u64> = ctx
        .db
        .zones()
        .zone_id()
        .filter(zone_id)
        .filter(|z| valid_at_time(z.valid_at) > time_ms)
        .map(|z| z.valid_at)
        .collect();
    future.sort_unstable_by_key(|v| valid_at_time(*v));
    for v in future {
        let Some(z) = ctx.db.zones().valid_at().find(v) else {
            continue;
        };
        let cur_slot = z.tile_at(row, col).unwrap_or((0, 0, 0));
        if cur_slot != old_slot {
            break;
        }
        let mut updated = z;
        updated.assign_tile(row, col, def_id, stock0, stock1);
        ctx.db.zones().valid_at().delete(v);
        ctx.db.zones().insert(updated);
    }

    Some(written)
}

/// Look up the declared `stock.default` values for a packed tile
/// def. Returns `(0, 0)` when the def is unknown or declares no
/// stock slots. Shared by every zone-generation path so the
/// "freshly-placed tile starts at its def's defaults" rule has one
/// source of truth — worldgen seeds tiles this way, and
/// `action_completion` uses it when a recipe spawns a tile via
/// `ProductPlace::Location`.
///
/// `packed_def` is the full `[card_type:u4 | def_id:u12]` value.
/// Callers with only a bare tile def_id should combine it with the
/// tile card_type first (`pack_definition(7, def_id)` for world
/// tiles); zones don't carry a card_type axis themselves.
pub fn stock_defaults_for(packed_def: u16) -> (u8, u8) {
    resonantdust_content::definition_core::decode_definition(packed_def)
        .ok()
        .flatten()
        .map(|d| {
            let s0 = d.stock.first().map(|s| s.default).unwrap_or(0);
            let s1 = d.stock.get(1).map(|s| s.default).unwrap_or(0);
            (s0, s1)
        })
        .unwrap_or((0, 0))
}

/// `set_tile_at` variant that seeds the stock bits from the def's
/// declared defaults. Use this for fresh placements (worldgen,
/// product spawns); use the explicit-stock [`set_tile_at`] when
/// the caller is preserving / overriding row values (mutation
/// passes, biome reverts that explicitly clear stocks).
pub fn set_tile_at_with_defaults(
    ctx: &ReducerContext,
    zone_id: u32,
    time_ms: u64,
    row: u8,
    col: u8,
    packed_def: u16,
) -> Option<Zone> {
    let (s0, s1) = stock_defaults_for(packed_def);
    set_tile_at(ctx, zone_id, time_ms, row, col, packed_def & 0x0FFF, s0, s1)
}

/// Zone disk footprint: a radius-3 hex disk (37 cells) carved out
/// of a 7×7 sub-region of the 8×8 zone-tile grid, centered at
/// `(row=3, col=3)`. The 6 top-left and 6 bottom-right corners of
/// the 7×7 area fall outside the disk; row 7 and column 7 of the
/// 8×8 storage layout are unused. Matches the mini_zone footprint
/// (see `mini_zone.rs`).
const DISK_CENTER: i32 = 3;
const DISK_RADIUS: i32 = 3;

/// True iff `(row, col)` lies within the radius-3 hex disk centered
/// at `(3, 3)`. Cube-distance via the axial-coord identity
/// `d = (|dq| + |dr| + |dq+dr|) / 2`.
fn in_disk(row: u8, col: u8) -> bool {
    let dr = row as i32 - DISK_CENTER;
    let dq = col as i32 - DISK_CENTER;
    (dq.abs() + dr.abs() + (dq + dr).abs()) / 2 <= DISK_RADIUS
}

/// Create a fresh "zone disk" — a Zone whose 37 in-disk cells are
/// seeded with the `"empty"` tile definition (plus its declared
/// stock defaults) and whose remaining 27 backing slots stay 0.
/// Allocates a new `zone_id`, stamps the row at `time_ms`.
///
/// The Zone's `packed_definition` advertises the tile card_type,
/// so `tile_at`-derived synthetic hexes resolve through the recipe
/// matcher the same way world-zone tiles do. The card_type is
/// derived from the looked-up `"empty"` packed_def rather than
/// hardcoded, so it tracks `cards/types.json` if the tile id ever
/// shifts.
pub fn create_disk_at(
    ctx: &ReducerContext,
    macro_zone: u64,
    owner_id: u32,
    time_ms: u64,
) -> Result<Zone, String> {
    let empty_packed = find_packed_by_key("empty")?
        .ok_or_else(|| "create_disk_at: tile def \"empty\" not in content catalog".to_string())?;
    let card_type = (empty_packed >> 12) as u8;
    let def_id = empty_packed & packed::DEF_ID_MASK;
    let (s0, s1) = stock_defaults_for(empty_packed);

    let mut tiles = [0u64; packed::ZONE_TILE_U64_COUNT];
    for row in 0u8..7 {
        for col in 0u8..7 {
            if !in_disk(row, col) {
                continue;
            }
            packed::set_tile_full(&mut tiles, (row * 8 + col) as usize, def_id, s0, s1);
        }
    }

    let zone_id = next_zone_id(ctx);
    Ok(create_at(
        ctx,
        zone_id,
        macro_zone,
        pack_zone_definition(card_type),
        owner_id,
        tiles,
        time_ms,
    ))
}

/// Create a Zone whose full 8×8 grid is seeded with the `"empty"` tile
/// definition (+ its declared stock defaults) — the dense rect-grid analogue
/// of [`create_disk_at`]. Used for **inventory** zones: one ~153-byte Zone row
/// replaces 64 ~40-byte empty tile cards (and tile-cards spawn on demand via
/// `find_or_create_tile_card` only when a recipe touches a cell — exactly the
/// world model). A rect viewport addresses all 64 cells, so there's no in-disk
/// carve. Allocates a fresh `zone_id`, stamps the row at `time_ms`.
pub fn create_rect_at(
    ctx: &ReducerContext,
    macro_zone: u64,
    owner_id: u32,
    time_ms: u64,
) -> Result<Zone, String> {
    let empty_packed = find_packed_by_key("empty")?
        .ok_or_else(|| "create_rect_at: tile def \"empty\" not in content catalog".to_string())?;
    let card_type = (empty_packed >> 12) as u8;
    let def_id = empty_packed & packed::DEF_ID_MASK;
    let (s0, s1) = stock_defaults_for(empty_packed);

    let mut tiles = [0u64; packed::ZONE_TILE_U64_COUNT];
    for idx in 0..64usize {
        packed::set_tile_full(&mut tiles, idx, def_id, s0, s1);
    }

    let zone_id = next_zone_id(ctx);
    Ok(create_at(
        ctx,
        zone_id,
        macro_zone,
        pack_zone_definition(card_type),
        owner_id,
        tiles,
        time_ms,
    ))
}

/// Surgical stock-only mutation: change `slot` (0 or 1) at
/// `(row, col)` to `value`, preserving the tile's def_id and the
/// other stock slot. Shares forward-prop discipline with
/// [`set_tile_at`] — the propagator compares the full tile slot, so
/// a stock-only write counts as a deliberate change against later
/// rows.
pub fn set_tile_stock_at(
    ctx: &ReducerContext,
    zone_id: u32,
    time_ms: u64,
    row: u8,
    col: u8,
    slot: usize,
    value: u8,
) -> Option<Zone> {
    if row >= 8 || col >= 8 || slot >= packed::ZONE_TILE_STOCK_SLOTS {
        return None;
    }
    let prior = ctx
        .db
        .zones()
        .zone_id()
        .filter(zone_id)
        .filter(|z| valid_at_time(z.valid_at) <= time_ms)
        .max_by_key(|z| valid_at_time(z.valid_at))?;
    let (def_id, s0, s1) = prior.tile_at(row, col).unwrap_or((0, 0, 0));
    let (new_s0, new_s1) = if slot == 0 { (value, s1) } else { (s0, value) };
    set_tile_at(ctx, zone_id, time_ms, row, col, def_id, new_s0, new_s1)
}
