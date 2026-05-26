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
    pub surface: u8,
    #[index(btree)]
    pub macro_zone: u32,
    pub packed_definition: u8,
    /// Container id of every synthetic hex derived from this
    /// zone's tile bytes. Usually a `card_id` (consistent with
    /// `Card.owner_id` semantics when `FLAG_OWNED_BY_PLAYER` is
    /// clear). `0` is the WORLD sentinel — world zones use it, and
    /// `ProductOwner::Hex` resolution falls back to the caller's
    /// soul when this is `0`. Mini_zones store `anchor.card_id`
    /// here.
    ///
    /// **Player-dimension exception.** On
    /// `surface == PLAYER_DIMENSION_LAYER (62)`, `owner_id` is the
    /// `player_id` (not a card_id). The surface band has multiple
    /// Zones at the same `(surface, macro_zone)` — one per player
    /// — and `owner_id` is the discriminator. Btree-indexed so
    /// owner-keyed lookups stay O(log N) at 1000+ players.
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

/// Latest row for the zone at `(surface, macro_zone)`. The
/// `(surface, macro_zone)` tuple identifies a container — world
/// chunks at `surface=WORLD_LAYER`, mini_zones at
/// `surface=MINI_ZONE_LAYER` (with `macro_zone = anchor.card_id`),
/// pocket dimensions at `surface=POCKET_DIMENSION_LAYER`. Returns
/// `None` if no Zone exists at that address.
///
/// **Do not use on `surface == PLAYER_DIMENSION_LAYER`.** That band
/// has multiple Zones per `(surface, macro_zone)` — one per player
/// — and this function would return whichever row has the highest
/// `valid_at_time`, ignoring owner. Use [`latest_for_owner`] there.
pub fn latest_for(ctx: &ReducerContext, surface: u8, macro_zone: u32) -> Option<Zone> {
    ctx.db
        .zones()
        .macro_zone()
        .filter(macro_zone)
        .filter(|z| z.surface == surface)
        .max_by_key(|z| valid_at_time(z.valid_at))
}

/// Owner-discriminated zone lookup — returns the latest row matching
/// all three of `(surface, macro_zone, owner_id)`. Required on
/// `PLAYER_DIMENSION_LAYER` where the same `(surface, macro_zone)`
/// is shared across players, and `owner_id == player_id` is the
/// discriminator. Safe to use on other surfaces too (where there's
/// only ever one Zone per `(surface, macro_zone)` and the owner_id
/// filter is just a sanity check).
///
/// Scans via the `owner_id` btree — for surface 62 with N players,
/// each player has ≤ 4 Zones (one per chunk in their 2×2 dimension),
/// so this is O(player's-own-zones) regardless of total player
/// count. The world surface uses `owner_id=0` for all rows; calling
/// this on world would scan every world Zone, so don't.
pub fn latest_for_owner(
    ctx: &ReducerContext,
    surface: u8,
    macro_zone: u32,
    owner_id: u32,
) -> Option<Zone> {
    ctx.db
        .zones()
        .owner_id()
        .filter(owner_id)
        .filter(|z| z.surface == surface && z.macro_zone == macro_zone)
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
    surface: u8,
    macro_zone: u32,
    packed_definition: u8,
    owner_id: u32,
    tiles: [u64; packed::ZONE_TILE_U64_COUNT],
) -> Zone {
    create_at(ctx, zone_id, surface, macro_zone, packed_definition, owner_id, tiles, now_ms(ctx))
}

// Insert a brand-new zone stamped at `time_ms`. Mirrors
// `cards::create_at` — use this from reducers that have resolved an
// `effective_now_ms` value so the row's `valid_at` matches the
// client's view of "now".
#[allow(clippy::too_many_arguments)]
pub fn create_at(
    ctx: &ReducerContext,
    zone_id: u32,
    surface: u8,
    macro_zone: u32,
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
            surface,
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

pub fn set_surface(ctx: &ReducerContext, zone_id: u32, surface: u8) -> Option<Zone> {
    update_with(ctx, zone_id, |z| z.surface = surface)
}

pub fn set_macro_zone(ctx: &ReducerContext, zone_id: u32, macro_zone: u32) -> Option<Zone> {
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
    surface: u8,
    macro_zone: u32,
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
        surface,
        macro_zone,
        pack_zone_definition(card_type),
        owner_id,
        tiles,
        time_ms,
    ))
}

/// Side length of each player's pocket dimension, measured in 8×8
/// chunks. 2 = a 2×2 grid of `Zone`s = 256 total tiles. Tuned for
/// "snug but workable" at 10-15 characters per player; bumping this
/// requires either a content migration for existing players or a
/// runtime "expand my dimension" reducer.
pub const PLAYER_DIMENSION_GRID_SIZE: i16 = 2;

/// Eager-create a fresh player pocket dimension — a 2×2 grid of
/// full-8×8 Zones at `(surface=PLAYER_DIMENSION_LAYER, macro_zone=
/// pack(q, r), owner_id=player_id)`, each filled with the `"empty"`
/// tile def + its declared stock defaults so souls can walk on them
/// (`def_id=0` is impassable per `movement.rs`).
///
/// Called once per new player from `claim_or_login`'s new-player
/// branch. Idempotency NOT enforced here — re-running for an
/// existing player creates a second set of Zones at the same
/// addresses. The lookup path (`latest_for_owner`) would then
/// return the latest, but the older rows linger until GC. The
/// caller must gate.
///
/// The `Zone.owner_id` discriminator is the `player_id`, which is
/// the surface 62 convention (not a card_id — see the doc-comment
/// on `Zone.owner_id`).
pub fn create_player_dimension(
    ctx: &ReducerContext,
    player_id: u32,
    time_ms: u64,
) -> Result<(), String> {
    // Ring tiles: `concrete` — paved, walkable, faction-tinted via
    // the `texture: { aspect: "concrete" }` field on the def. The
    // `card_type` lookup uses concrete since every seed tile shares
    // the tile card_type; this just picks one to derive it from.
    let concrete_packed = find_packed_by_key("concrete")?.ok_or_else(|| {
        "create_player_dimension: tile def \"concrete\" not in content catalog".to_string()
    })?;
    let card_type = (concrete_packed >> 12) as u8;
    let concrete_def_id = concrete_packed & packed::DEF_ID_MASK;
    let (concrete_s0, concrete_s1) = stock_defaults_for(concrete_packed);

    // The center seed tile is the `alter` — every player's pocket
    // dimension starts with one alter at the heart of the seed ring.
    // Future level changes upgrade this tile's def (alter is keyed on
    // `aspects.level`, so each level is its own def).
    let alter_packed = find_packed_by_key("alter")?.ok_or_else(|| {
        "create_player_dimension: tile def \"alter\" not in content catalog".to_string()
    })?;
    let alter_def_id = alter_packed & packed::DEF_ID_MASK;
    let (alter_s0, alter_s1) = stock_defaults_for(alter_packed);

    // Two hexes east of the alter is the `anima_fountain`; two hexes
    // west (opposite) is the `aether_fountain`. Both share the
    // `fountain` object pack (per-card `index` picks the sprite) and
    // sit on the same concrete tile texture as the alter. Add more
    // fountain types here as the family grows.
    let anima_fountain_packed = find_packed_by_key("anima_fountain")?.ok_or_else(|| {
        "create_player_dimension: tile def \"anima_fountain\" not in content catalog"
            .to_string()
    })?;
    let anima_fountain_def_id = anima_fountain_packed & packed::DEF_ID_MASK;
    let (anima_fountain_s0, anima_fountain_s1) = stock_defaults_for(anima_fountain_packed);

    let aether_fountain_packed = find_packed_by_key("aether_fountain")?.ok_or_else(|| {
        "create_player_dimension: tile def \"aether_fountain\" not in content catalog"
            .to_string()
    })?;
    let aether_fountain_def_id = aether_fountain_packed & packed::DEF_ID_MASK;
    let (aether_fountain_s0, aether_fountain_s1) = stock_defaults_for(aether_fountain_packed);

    // Two `table` tiles flank the alter — one southeast (dq=0, dr=1),
    // one southwest (dq=-1, dr=1). Same def for both positions; we
    // pass the fixture once and the builder drops it at both offsets.
    let table_packed = find_packed_by_key("table")?.ok_or_else(|| {
        "create_player_dimension: tile def \"table\" not in content catalog".to_string()
    })?;
    let table_def_id = table_packed & packed::DEF_ID_MASK;
    let (table_s0, table_s1) = stock_defaults_for(table_packed);

    // Per-chunk seed pattern: a filled hex disc of radius
    // `PLAYER_DIM_SEED_RADIUS` (cell count = 1 + 3·R·(R+1) — so 7
    // cells at R=1, 19 at R=2, 37 at R=3) centred on local (3, 3).
    // The rest of the 8×8 grid stays `def_id == 0` (impassable per
    // `movement.rs`). Authoring intent: each chunk starts as a small
    // outpost; players grow it outward via build recipes. Bump
    // `PLAYER_DIM_SEED_RADIUS` to start players with more room.
    //
    // Axial-distance test for pointy-top hex coords:
    //   dist(dq, dr) = (|dq| + |dr| + |dq + dr|) / 2
    // The alter sits at (dq=1, dr=-2) — local (4, 1) — within the
    // disc but offset from its centre; two hexes east of the disc
    // centre (dq=2, dr=0) is the anima fountain; two hexes west
    // (dq=-2, dr=0) is the aether fountain; one hex southeast
    // (dq=0, dr=1) and one hex southwest (dq=-1, dr=1) are tables
    // (same def at both positions); every other in-disc cell is
    // concrete. Corpse cards are seeded onto the concrete tiles
    // only (see `players::seed_player_dim_corpses`).
    const PLAYER_DIM_SEED_CENTER: (u8, u8) = (3, 3); // (q, r) local
    const PLAYER_DIM_SEED_RADIUS: i8 = 2;

    // Only the "home" chunk (0, 0) gets a seeded layout — alter +
    // fountain inside a concrete disc. The other three chunks spawn
    // empty (`def_id == 0` everywhere, impassable per `movement.rs`);
    // players carve them out as the dimension grows. Fixtures are
    // unique-per-dim by design — duplicating the alter across every
    // chunk would make rituals like `bio_attune` (which targets THE
    // alter via `Hex.0.owner.aspect.faction.set`) ambiguous.
    let home_tiles = build_dim_tiles(
        PLAYER_DIM_SEED_CENTER,
        PLAYER_DIM_SEED_RADIUS,
        concrete_def_id,
        concrete_s0,
        concrete_s1,
        Some((alter_def_id, alter_s0, alter_s1)),
        Some((anima_fountain_def_id, anima_fountain_s0, anima_fountain_s1)),
        Some((aether_fountain_def_id, aether_fountain_s0, aether_fountain_s1)),
        Some((table_def_id, table_s0, table_s1)),
    );
    let empty_tiles = [0u64; packed::ZONE_TILE_U64_COUNT];

    let zone_packed_def = pack_zone_definition(card_type);
    // Allocate the 4 zone_ids contiguously. `next_zone_id` is O(N)
    // per call so we take one snapshot and increment locally — safe
    // because the reducer transaction is atomic.
    let base_zone_id = next_zone_id(ctx);
    let mut offset: u32 = 0;
    for chunk_q in 0..PLAYER_DIMENSION_GRID_SIZE {
        for chunk_r in 0..PLAYER_DIMENSION_GRID_SIZE {
            let macro_zone = packed::pack_macro_zone(chunk_q, chunk_r);
            let tiles = if chunk_q == 0 && chunk_r == 0 {
                home_tiles
            } else {
                empty_tiles
            };
            create_at(
                ctx,
                base_zone_id + offset,
                packed::PLAYER_DIMENSION_LAYER,
                macro_zone,
                zone_packed_def,
                player_id,
                tiles,
                time_ms,
            );
            offset += 1;
        }
    }
    Ok(())
}

/// Build a per-chunk tile bitfield from the shared hex-disc seed
/// pattern. `centre_alter` / `anima_fountain` / `aether_fountain` /
/// `table` are the fixture def-and-stocks to drop at the
/// `(1, -2)`, `(2, 0)`, `(-2, 0)`, and `{(0, 1), (-1, 1)}` offsets
/// from `centre`; pass `None` for chunks that should stay
/// all-concrete (no fixtures). The `table` slot drops the same
/// fixture at both southeast and southwest positions. The name
/// `centre_alter` is historical — the alter no longer sits at the
/// disc centre but kept the name to avoid a churn-only rename.
/// Everything else in the disc is concrete. Cells outside the
/// disc and outside `0..8` stay `def_id == 0` (impassable per
/// `movement.rs`).
fn build_dim_tiles(
    centre: (u8, u8),
    radius: i8,
    concrete_def_id: u16,
    concrete_s0: u8,
    concrete_s1: u8,
    centre_alter: Option<(u16, u8, u8)>,
    anima_fountain: Option<(u16, u8, u8)>,
    aether_fountain: Option<(u16, u8, u8)>,
    table: Option<(u16, u8, u8)>,
) -> [u64; packed::ZONE_TILE_U64_COUNT] {
    let mut tiles = [0u64; packed::ZONE_TILE_U64_COUNT];
    for dq in -radius..=radius {
        for dr in -radius..=radius {
            let dist = (dq.abs() + dr.abs() + (dq + dr).abs()) / 2;
            if dist > radius {
                continue;
            }
            let q = centre.0 as i8 + dq;
            let r = centre.1 as i8 + dr;
            if q < 0 || q >= 8 || r < 0 || r >= 8 {
                continue;
            }
            let idx = (r as usize) * 8 + (q as usize);
            let (def_id, s0, s1) = match (dq, dr) {
                (1, -2) if centre_alter.is_some() => centre_alter.unwrap(),
                (2, 0) if anima_fountain.is_some() => anima_fountain.unwrap(),
                (-2, 0) if aether_fountain.is_some() => aether_fountain.unwrap(),
                (0, 1) if table.is_some() => table.unwrap(),
                (-1, 1) if table.is_some() => table.unwrap(),
                _ => (concrete_def_id, concrete_s0, concrete_s1),
            };
            packed::set_tile_full(&mut tiles, idx, def_id, s0, s1);
        }
    }
    tiles
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
