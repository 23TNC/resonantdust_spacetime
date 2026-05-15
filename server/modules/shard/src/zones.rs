use spacetimedb::{table, ReducerContext, Table};

use crate::packed::{self, pack_valid_at, valid_at_time};
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
    /// Container card_id of every synthetic hex derived from this
    /// zone's tile bytes. Under the post-flag-20 card-owner model,
    /// `Zone.owner_id` is always a `card_id` (consistent with
    /// `Card.owner_id` semantics when `FLAG_OWNED_BY_PLAYER` is
    /// clear). `0` is the WORLD sentinel — world zones use it, and
    /// `ProductOwner::Hex` resolution falls back to the caller's
    /// soul when this is `0`. Mini_zones store `anchor.card_id`
    /// here; future pocket-dimension zones (`surface ∈ 32..63`) can
    /// similarly carry their anchor's card_id so tile-as-hex outputs
    /// inherit the right container.
    pub owner_id: u32,
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
pub fn latest_for(ctx: &ReducerContext, surface: u8, macro_zone: u32) -> Option<Zone> {
    ctx.db
        .zones()
        .macro_zone()
        .filter(macro_zone)
        .filter(|z| z.surface == surface)
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
            owner_id,
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

// Replace one byte (one hex def_id) within row `row` at column `col`
// (each 0..8). Stamps the new version row at *now*. Delegates to
// `set_tile_at` so both the prior-row read and the forward-propagation
// of the change apply uniformly — wall-clock writes are just the
// `time_ms = now_ms` special case.
pub fn set_tile(
    ctx: &ReducerContext,
    zone_id: u32,
    row: u8,
    col: u8,
    def_id: u8,
) -> Option<Zone> {
    set_tile_at(ctx, zone_id, now_ms(ctx), row, col, def_id)
}

/// Write a single tile byte at `(row, col)` to `def_id`, stamping the
/// new Zone version row at `time_ms`.
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
///    order. For each, if its byte at `(row, col)` still equals the
///    prior row's byte (i.e., the tile was inherited from before our
///    write — nobody deliberately changed it after us), overwrite it
///    with `def_id`. Stop at the first row where the byte already
///    differs — that's a deliberate change made by another action;
///    clobbering it would corrupt the other action's outcome.
///
/// The "stop on first deliberate change" rule keeps the propagation
/// safe in the presence of multiple actions on the same tile across
/// different times. The current world model doesn't let two players
/// target the same tile yet, but the safety net is cheap and prevents
/// a class of bugs from showing up later.
pub fn set_tile_at(
    ctx: &ReducerContext,
    zone_id: u32,
    time_ms: u64,
    row: u8,
    col: u8,
    def_id: u8,
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
    let old_def_id = packed::tile_byte(prior.tile_row(row).unwrap_or(0), col as usize);

    // If the prior row already has this byte, our write is a no-op at
    // the baseline. Skip the write but still run forward-propagation —
    // there may be future rows that diverged for some other reason and
    // we'd want to leave them alone (the propagation's stop-on-change
    // rule does that), so this is a clean early-return.
    if old_def_id == def_id {
        return Some(prior);
    }

    // Build the new row on top of `prior`'s data + our one-byte change,
    // then write at `time_ms`.
    let cur = prior.tile_row(row).unwrap_or(0);
    let next = packed::with_tile_byte(cur, col as usize, def_id);
    prior.assign_tile_row(row, next);
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
        let row_bytes = z.tile_row(row).unwrap_or(0);
        let cur_byte = packed::tile_byte(row_bytes, col as usize);
        if cur_byte != old_def_id {
            // Deliberate change in this row (or one before it that we
            // already passed) — stop. Anything after this row is
            // presumed downstream of *that* deliberate change, not
            // ours, so we leave it alone.
            break;
        }
        let mut updated = z;
        let new_row_bytes = packed::with_tile_byte(row_bytes, col as usize, def_id);
        updated.assign_tile_row(row, new_row_bytes);
        ctx.db.zones().valid_at().delete(v);
        ctx.db.zones().insert(updated);
    }

    Some(written)
}
