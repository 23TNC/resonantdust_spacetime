use spacetimedb::{table, ReducerContext, Table};

use crate::packed::{self, pack_valid_at, valid_at_time};

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
    pub t0: u64,
    pub t1: u64,
    pub t2: u64,
    pub t3: u64,
    pub t4: u64,
    pub t5: u64,
    pub t6: u64,
    pub t7: u64,
}

impl Zone {
    pub fn tile_row(&self, row: u8) -> Option<u64> {
        Some(match row {
            0 => self.t0,
            1 => self.t1,
            2 => self.t2,
            3 => self.t3,
            4 => self.t4,
            5 => self.t5,
            6 => self.t6,
            7 => self.t7,
            _ => return None,
        })
    }

    pub fn assign_tile_row(&mut self, row: u8, value: u64) -> bool {
        match row {
            0 => self.t0 = value,
            1 => self.t1 = value,
            2 => self.t2 = value,
            3 => self.t3 = value,
            4 => self.t4 = value,
            5 => self.t5 = value,
            6 => self.t6 = value,
            7 => self.t7 = value,
            _ => return false,
        }
        true
    }
}

fn now_secs(ctx: &ReducerContext) -> u32 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000_000) as u32
}

// Latest row for a zone_id is the row with the largest time component of valid_at.
pub fn latest(ctx: &ReducerContext, zone_id: u32) -> Option<Zone> {
    ctx.db
        .zones()
        .zone_id()
        .filter(zone_id)
        .max_by_key(|z| valid_at_time(z.valid_at))
}

// Stamp valid_at = (zone_id, now) and write. If a row already exists at that
// exact key (two writes in the same second), the existing one is replaced —
// "always accept the most recent write".
fn write(ctx: &ReducerContext, mut zone: Zone) -> Zone {
    zone.valid_at = pack_valid_at(zone.zone_id, now_secs(ctx));
    if ctx.db.zones().valid_at().find(zone.valid_at).is_some() {
        ctx.db.zones().valid_at().delete(zone.valid_at);
    }
    ctx.db.zones().insert(zone)
}

// Insert a brand-new zone. valid_at is computed; pass 0 will be overwritten.
pub fn create(
    ctx: &ReducerContext,
    zone_id: u32,
    surface: u8,
    macro_zone: u32,
    packed_definition: u8,
    tiles: [u64; 8],
) -> Zone {
    write(
        ctx,
        Zone {
            valid_at: 0,
            zone_id,
            surface,
            macro_zone,
            packed_definition,
            t0: tiles[0],
            t1: tiles[1],
            t2: tiles[2],
            t3: tiles[3],
            t4: tiles[4],
            t5: tiles[5],
            t6: tiles[6],
            t7: tiles[7],
        },
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

// Replace all 8 tiles in row `row` (0..8).
pub fn set_tile_row(ctx: &ReducerContext, zone_id: u32, row: u8, value: u64) -> Option<Zone> {
    if row >= 8 {
        return None;
    }
    update_with(ctx, zone_id, |z| {
        z.assign_tile_row(row, value);
    })
}

// Replace all 8 tile rows at once.
pub fn set_tile_rows(ctx: &ReducerContext, zone_id: u32, tiles: [u64; 8]) -> Option<Zone> {
    update_with(ctx, zone_id, |z| {
        z.t0 = tiles[0];
        z.t1 = tiles[1];
        z.t2 = tiles[2];
        z.t3 = tiles[3];
        z.t4 = tiles[4];
        z.t5 = tiles[5];
        z.t6 = tiles[6];
        z.t7 = tiles[7];
    })
}

// Replace one byte (one hex def_id) within row `row` at column `col` (each 0..8).
pub fn set_tile(
    ctx: &ReducerContext,
    zone_id: u32,
    row: u8,
    col: u8,
    def_id: u8,
) -> Option<Zone> {
    if row >= 8 || col >= 8 {
        return None;
    }
    update_with(ctx, zone_id, |z| {
        let cur = z.tile_row(row).unwrap_or(0);
        let next = packed::with_tile_byte(cur, col as usize, def_id);
        z.assign_tile_row(row, next);
    })
}
