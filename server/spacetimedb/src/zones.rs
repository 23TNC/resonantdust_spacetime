//! Zones table — bulk world-tile storage.
//!
//! World tiles are dense (an 8×8 chunk = 64 cells per zone) and within
//! a chunk most tiles share the same `(card_type, card_category)` —
//! they're all "world tile" cards of the same category. A per-tile
//! [`Card`](crate::cards::Card) row would be 64× the bookkeeping for
//! one chunk's worth of identity. Instead, a [`Zone`] row stores the
//! whole chunk in three fields:
//!
//! - `macro_zone`           — chunk id, matches `Card.macro_zone` for
//!                            any world card sitting in this chunk.
//!                            Primary key.
//! - `packed_definition`    — `[card_type:u4][card_category:u4]`. The
//!                            high byte of every cell's full
//!                            `u16 packed_definition`. Shared by every
//!                            cell in the zone — vary the
//!                            `definition_id` to vary the card.
//! - `packed_definition_ids` — eight `u64`s, byte-packed: each `u64`
//!                            holds eight cell `definition_id`s, low
//!                            byte first. 8 × 8 = 64 cells total.
//!
//! A cell's full `u16 packed_definition` is rebuilt on read as
//! `((zone.packed_definition as u16) << 8) | (cell_definition_id as u16)`.
//!
//! # Cell addressing
//!
//! A cell sits at `(local_q: u3, local_r: u3)` — the same nibbles
//! that occupy the high 6 bits of a world card's [`Card.micro_zone`]
//! byte (`bits 7..5 = local_q`, `bits 4..2 = local_r`, low 2 bits
//! reserved for `stack_state`).
//!
//! Flat cell index is **row-major**:
//!
//! ```text
//!   cell_index   = local_r * 8 + local_q          // 0..64
//!   u64_index    = cell_index / 8                  // 0..8
//!   byte_offset  = cell_index % 8                  // 0..8 (one byte per cell)
//!   id           = (packed_definition_ids[u64_index] >> (byte_offset * 8)) & 0xFF
//! ```
//!
//! With row-major addressing each `u64` holds one full row of eight
//! cells — handy for "rewrite a row" operations and for visualizing
//! the layout when debugging.
//!
//! # Empty cells
//!
//! `definition_id == 0` is the empty-cell sentinel (matches the
//! `Card`-side reservation in `cards/id.json`). A freshly-inserted
//! [`Zone`] has all eight `u64`s zeroed and represents an empty chunk.
//!
//! # Why no per-cell `Card` rows?
//!
//! World tiles don't move, don't carry per-tile mutable state in this
//! POC, and outnumber inventory cards by orders of magnitude. Storing
//! one row per tile would blow up the cards table without buying any
//! flexibility we need. When a tile *does* need first-class card
//! state (a tree being chopped, a creature occupying a tile), the
//! plan is to materialize a real [`Card`] row at that micro_zone and
//! treat the zone cell as the "default" appearance until the row goes
//! away again. Today nothing does this; the zone is the sole source
//! of truth for world tile identity.

use spacetimedb::{ReducerContext, Table};

// ─── Constants ───────────────────────────────────────────────────────────────

/// Side length of one zone's grid (cells per row, cells per column).
pub const ZONE_SIDE: u8 = 8;
/// Total cells per zone (`ZONE_SIDE * ZONE_SIDE`).
pub const ZONE_CELL_COUNT: usize = (ZONE_SIDE as usize) * (ZONE_SIDE as usize);
/// How many `definition_id` bytes pack into one `u64` (== 8).
pub const CELLS_PER_U64: usize = 8;
/// Number of `u64`s in a zone's `packed_definition_ids` vector.
pub const ZONE_U64_COUNT: usize = ZONE_CELL_COUNT / CELLS_PER_U64;
/// Mask covering the 3 bits of one local-coord component.
pub const LOCAL_COORD_MASK: u8 = 0b111;
/// Empty-cell sentinel: `definition_id == 0` means "no tile here".
pub const EMPTY_CELL: u8 = 0;

// ─── Table ───────────────────────────────────────────────────────────────────

/// One world chunk. Public — clients subscribe by
/// `(layer, macro_zone)` to render the world around them.
///
/// The natural identity of a chunk is the `(layer, macro_zone)`
/// pair: two zones with the same `macro_zone` on different layers
/// (overworld / dream / underworld / …) coexist as distinct rows.
/// Lookups that already have a Card in hand should pass
/// `(card.layer, card.macro_zone)` via [`find_zone`]; the
/// `macro_zone` btree index keeps the filter cheap.
#[spacetimedb::table(accessor = zones, public)]
#[derive(Debug, Clone)]
pub struct Zone {
  /// Synthetic auto-incrementing PK. Identity is logically
  /// `(layer, macro_zone)`; this column exists only because
  /// SpacetimeDB tables need a single-field primary key. Clients
  /// shouldn't subscribe on this — filter by `(layer, macro_zone)`.
  #[primary_key]
  #[auto_inc]
  pub zone_id: u32,
  /// World layer this chunk lives on. Matches `Card.layer` for any
  /// world card occupying a cell in this chunk.
  #[index(btree)]
  pub layer: u8,
  /// World chunk identifier within the layer. Matches
  /// `Card.macro_zone` for any world card occupying a cell here.
  #[index(btree)]
  pub macro_zone: u32,
  /// `[card_type:u4][card_category:u4]` shared by every cell in the
  /// zone. Combined with a cell's `definition_id` to form the full
  /// `u16 packed_definition` for the tile sitting in that cell.
  pub packed_definition: u8,
  /// Byte-packed cell `definition_id`s, one `u64` per row (8 cells each,
  /// low byte first). Use [`read_cell`] / [`write_cell`] via
  /// [`Zone::cell_rows`] / [`Zone::set_cell_rows`] rather than touching
  /// these directly.
  pub t0: u64,
  pub t1: u64,
  pub t2: u64,
  pub t3: u64,
  pub t4: u64,
  pub t5: u64,
  pub t6: u64,
  pub t7: u64,
  /// Scheduled-reducer lag at the time of this row write, in 16-ms
  /// steps (saturating at 255). `0` for client-driven writes;
  /// non-zero only inside a scheduled reducer fire that's running
  /// late. See [`crate::delta_t`].
  pub delta_t: u8,
}

impl Zone {
  /// Extract the eight row words as an array for use with [`read_cell`]
  /// and [`write_cell`].
  pub fn cell_rows(&self) -> [u64; ZONE_U64_COUNT] {
    [self.t0, self.t1, self.t2, self.t3, self.t4, self.t5, self.t6, self.t7]
  }

  /// Write back a mutated row array produced by [`write_cell`].
  pub fn set_cell_rows(&mut self, rows: [u64; ZONE_U64_COUNT]) {
    self.t0 = rows[0]; self.t1 = rows[1]; self.t2 = rows[2]; self.t3 = rows[3];
    self.t4 = rows[4]; self.t5 = rows[5]; self.t6 = rows[6]; self.t7 = rows[7];
  }
}

// ─── Cell address ────────────────────────────────────────────────────────────

/// Zone-local cell coordinates `(q, r)`, both in `[0, 8)`. These are
/// the same 3-bit fields that occupy the high 6 bits of a world card's
/// [`Card.micro_zone`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalCoord {
  pub q: u8,
  pub r: u8,
}

impl LocalCoord {
  /// Build from raw coords. `Err` if either is `>= ZONE_SIDE`.
  pub fn new(q: u8, r: u8) -> Result<LocalCoord, String> {
    if q >= ZONE_SIDE || r >= ZONE_SIDE {
      return Err(format!(
        "local coord ({}, {}) out of range — both must be < {}",
        q, r, ZONE_SIDE,
      ));
    }
    Ok(LocalCoord { q, r })
  }

  /// Extract `(q, r)` from the high 6 bits of a `Card.micro_zone`
  /// byte. The low 2 bits (stack_state) are ignored.
  pub fn from_micro_zone(micro_zone: u8) -> LocalCoord {
    LocalCoord {
      q: (micro_zone >> 5) & LOCAL_COORD_MASK,
      r: (micro_zone >> 2) & LOCAL_COORD_MASK,
    }
  }

  /// Pack `(q, r)` into the high 6 bits of a `micro_zone` byte. The
  /// low 2 bits (stack_state) are zeroed; OR them in separately if
  /// needed.
  pub fn to_micro_zone(self) -> u8 {
    ((self.q & LOCAL_COORD_MASK) << 5) | ((self.r & LOCAL_COORD_MASK) << 2)
  }

  /// Flat cell index — row-major: `r * 8 + q`. Range 0..64.
  pub fn index(self) -> usize {
    (self.r as usize) * (ZONE_SIDE as usize) + (self.q as usize)
  }
}

// ─── Bit-level cell I/O ──────────────────────────────────────────────────────

/// Combine a zone's shared `packed_definition` byte with a cell's
/// `definition_id` to produce the full `u16 packed_definition` used by
/// `Card.packed_definition`. `cell_definition_id == 0` is the empty
/// sentinel — callers normally check that themselves and skip this
/// call rather than build a `0xZZ00` "empty" packed_definition.
#[inline]
pub fn cell_packed_definition(zone_packed_definition: u8, cell_definition_id: u8) -> u16 {
  ((zone_packed_definition as u16) << 8) | (cell_definition_id as u16)
}

/// Read a single cell's `definition_id` from the byte-packed array.
/// Returns [`EMPTY_CELL`] (`0`) for an empty cell.
///
/// Caller is responsible for `packed_ids.len() == ZONE_U64_COUNT`;
/// use the [`read_cell_checked`] wrapper if you can't guarantee that.
#[inline]
pub fn read_cell(packed_ids: &[u64], coord: LocalCoord) -> u8 {
  let cell_idx = coord.index();
  let word = packed_ids[cell_idx / CELLS_PER_U64];
  let shift = ((cell_idx % CELLS_PER_U64) * 8) as u32;
  ((word >> shift) & 0xFF) as u8
}

/// Write a single cell's `definition_id` into the byte-packed array
/// in place. The other seven cells in the same `u64` are preserved.
#[inline]
pub fn write_cell(packed_ids: &mut [u64], coord: LocalCoord, definition_id: u8) {
  let cell_idx = coord.index();
  let word = &mut packed_ids[cell_idx / CELLS_PER_U64];
  let shift = ((cell_idx % CELLS_PER_U64) * 8) as u32;
  let mask = !(0xFFu64 << shift);
  *word = (*word & mask) | ((definition_id as u64) << shift);
}

// ─── Public table helpers ────────────────────────────────────────────────────

/// Look up the [`Zone`] at `(layer, macro_zone)`. Filters the
/// `macro_zone` btree match by `layer` — multiple zones can share a
/// `macro_zone` value across layers, so we can't go directly through
/// a unique index. Returns the first matching row (uniqueness
/// across the pair is a soft invariant maintained by
/// [`insert_empty_zone`]).
pub fn find_zone(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
) -> Option<Zone> {
  ctx.db.zones().macro_zone().filter(&macro_zone).find(|z| z.layer == layer)
}

/// Insert (or replace) a [`Zone`] row with all cells empty. Returns
/// the inserted row. Idempotent on `(layer, macro_zone)` — calling
/// twice with the same pair overwrites whatever was there.
pub fn insert_empty_zone(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
  packed_definition: u8,
) -> Zone {
  if let Some(existing) = find_zone(ctx, layer, macro_zone) {
    ctx.db.zones().zone_id().delete(&existing.zone_id);
  }
  ctx.db.zones().insert(Zone {
    zone_id: 0,
    layer,
    macro_zone,
    packed_definition,
    t0: 0, t1: 0, t2: 0, t3: 0, t4: 0, t5: 0, t6: 0, t7: 0,
    delta_t: crate::delta_t::current(),
  })
}

/// Look up the cell at `coord` in zone `(layer, macro_zone)`,
/// returning the full `u16 packed_definition` of whatever tile is
/// sitting in it.
///
/// - `Ok(None)` — the zone doesn't exist, or the cell is empty
///                (`definition_id == 0`).
/// - `Err(_)`   — the zone row exists but has a malformed
///                `packed_definition_ids` length (data corruption).
pub fn lookup_cell(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
  coord: LocalCoord,
) -> Result<Option<u16>, String> {
  let Some(zone) = find_zone(ctx, layer, macro_zone) else {
    return Ok(None);
  };
  let rows = zone.cell_rows();
  let id = read_cell(&rows, coord);
  if id == EMPTY_CELL {
    return Ok(None);
  }
  Ok(Some(cell_packed_definition(zone.packed_definition, id)))
}

/// Set the cell at `coord` in zone `(layer, macro_zone)` to
/// `cell_definition_id`. Pass `0` ([`EMPTY_CELL`]) to clear a cell.
/// Errors if the zone row doesn't exist — callers that want lazy
/// creation should call [`insert_empty_zone`] first.
pub fn set_cell(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
  coord: LocalCoord,
  cell_definition_id: u8,
) -> Result<(), String> {
  let mut zone = find_zone(ctx, layer, macro_zone)
    .ok_or_else(|| format!("zone (layer={}, macro_zone={}) does not exist", layer, macro_zone))?;
  let mut rows = zone.cell_rows();
  write_cell(&mut rows, coord, cell_definition_id);
  zone.set_cell_rows(rows);
  zone.delta_t = crate::delta_t::current();
  ctx.db.zones().zone_id().update(zone);
  Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cell_index_layout() {
    // Row-major: same r ⇒ same u64 word, contiguous bytes.
    for r in 0..ZONE_SIDE {
      for q in 0..ZONE_SIDE {
        let coord = LocalCoord::new(q, r).unwrap();
        let idx = coord.index();
        assert_eq!(idx / CELLS_PER_U64, r as usize, "row {} q={}", r, q);
        assert_eq!(idx % CELLS_PER_U64, q as usize, "row {} q={}", r, q);
      }
    }
  }

  #[test]
  fn read_write_roundtrip() {
    let mut packed = [0u64; ZONE_U64_COUNT];
    for q in 0..ZONE_SIDE {
      for r in 0..ZONE_SIDE {
        let coord = LocalCoord::new(q, r).unwrap();
        let id = q.wrapping_mul(13).wrapping_add(r);
        write_cell(&mut packed, coord, id);
        assert_eq!(read_cell(&packed, coord), id);
      }
    }
    // After writing every cell, every cell should hold its expected
    // value — confirms writes don't clobber siblings in the same u64.
    for q in 0..ZONE_SIDE {
      for r in 0..ZONE_SIDE {
        let coord = LocalCoord::new(q, r).unwrap();
        let expected = q.wrapping_mul(13).wrapping_add(r);
        assert_eq!(read_cell(&packed, coord), expected);
      }
    }
  }

  #[test]
  fn empty_cell_is_zero() {
    let packed = [0u64; ZONE_U64_COUNT];
    for q in 0..ZONE_SIDE {
      for r in 0..ZONE_SIDE {
        let coord = LocalCoord::new(q, r).unwrap();
        assert_eq!(read_cell(&packed, coord), EMPTY_CELL);
      }
    }
  }

  #[test]
  fn cell_packed_definition_combines_high_low() {
    assert_eq!(cell_packed_definition(0xAB, 0x42), 0xAB42);
    assert_eq!(cell_packed_definition(0x00, 0x42), 0x0042);
    assert_eq!(cell_packed_definition(0xFF, 0x00), 0xFF00);
    assert_eq!(cell_packed_definition(0x12, 0xCD), 0x12CD);
  }

  #[test]
  fn micro_zone_roundtrip() {
    // Round-trip every (q, r) pair through micro_zone packing. The
    // low 2 bits (stack_state) of `to_micro_zone()` are zero — round
    // tripping with any non-zero stack_state requires the caller to
    // OR them back in, which `from_micro_zone` correctly ignores.
    for q in 0..ZONE_SIDE {
      for r in 0..ZONE_SIDE {
        let coord = LocalCoord::new(q, r).unwrap();
        let mz = coord.to_micro_zone();
        assert_eq!(LocalCoord::from_micro_zone(mz), coord);
        // And with any garbage stack_state mixed in.
        for stack_state in 0..=0b11u8 {
          let mz_with_state = mz | stack_state;
          assert_eq!(LocalCoord::from_micro_zone(mz_with_state), coord);
        }
      }
    }
  }

  #[test]
  fn local_coord_rejects_out_of_range() {
    assert!(LocalCoord::new(8, 0).is_err());
    assert!(LocalCoord::new(0, 8).is_err());
    assert!(LocalCoord::new(255, 0).is_err());
    assert!(LocalCoord::new(7, 7).is_ok());
  }

}
