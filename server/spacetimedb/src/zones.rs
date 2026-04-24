use crate::packing::pack_zone;
use spacetimedb::{reducer, ReducerContext, Table};

const ZONE_SIZE: u8 = 8;

#[spacetimedb::table(accessor = zones, public)]
#[derive(Debug, Clone)]
pub struct Zone {
  #[primary_key]
  pub zone: u32,
  pub definition: u8,
  pub t0: u64,
  pub t1: u64,
  pub t2: u64,
  pub t3: u64,
  pub t4: u64,
  pub t5: u64,
  pub t6: u64,
  pub t7: u64,
}

fn validate_local_coord(name: &str, value: u8) -> Result<(), String> {
  if value >= ZONE_SIZE {
    return Err(format!("{name} must be in 0..={}", ZONE_SIZE - 1));
  }
  Ok(())
}

fn validate_zone_definition(definition: u8) -> Result<(), String> {
  let card_type = definition >> 4;
  let definition_prefix = definition & 0x0F;

  if card_type > 0x0F {
    return Err(format!("zone card_type {} exceeds 4 bits", card_type));
  }
  if definition_prefix > 0x0F {
    return Err(format!(
      "zone definition_prefix {} exceeds 4 bits",
      definition_prefix
    ));
  }

  Ok(())
}

fn get_row(zone: &Zone, local_r: u8) -> u64 {
  match local_r {
    0 => zone.t0,
    1 => zone.t1,
    2 => zone.t2,
    3 => zone.t3,
    4 => zone.t4,
    5 => zone.t5,
    6 => zone.t6,
    7 => zone.t7,
    _ => unreachable!(),
  }
}

fn set_row(zone: &mut Zone, local_r: u8, value: u64) {
  match local_r {
    0 => zone.t0 = value,
    1 => zone.t1 = value,
    2 => zone.t2 = value,
    3 => zone.t3 = value,
    4 => zone.t4 = value,
    5 => zone.t5 = value,
    6 => zone.t6 = value,
    7 => zone.t7 = value,
    _ => unreachable!(),
  }
}

fn set_packed_tile(row: u64, local_q: u8, tile_def: u8) -> u64 {
  let shift = (local_q as u64) * 8;
  let mask = !(0xFFu64 << shift);
  (row & mask) | ((tile_def as u64) << shift)
}

fn make_filled_row(tile_def: u8) -> u64 {
  let b = tile_def as u64;
  b
    | (b << 8)
    | (b << 16)
    | (b << 24)
    | (b << 32)
    | (b << 40)
    | (b << 48)
    | (b << 56)
}

fn empty_zone(zone: u32, definition: u8) -> Zone {
  Zone {
    zone,
    definition,
    t0: 0,
    t1: 0,
    t2: 0,
    t3: 0,
    t4: 0,
    t5: 0,
    t6: 0,
    t7: 0,
  }
}

fn zone_from_coords(zone_q: i16, zone_r: i16, zone_z: u16) -> u32 {
  pack_zone(zone_q, zone_r, zone_z)
}

#[reducer]
pub fn upsert_zone(
  ctx: &ReducerContext,
  zone: u32,
  definition: u8,
  t0: u64,
  t1: u64,
  t2: u64,
  t3: u64,
  t4: u64,
  t5: u64,
  t6: u64,
  t7: u64,
) -> Result<(), String> {
  validate_zone_definition(definition)?;

  let row = Zone {
    zone,
    definition,
    t0,
    t1,
    t2,
    t3,
    t4,
    t5,
    t6,
    t7,
  };

  if ctx.db.zones().zone().find(&zone).is_some() {
    ctx.db.zones().zone().delete(zone);
  }
  ctx.db.zones().insert(row);

  Ok(())
}

#[reducer]
pub fn upsert_zone_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  zone_z: u16,
  definition: u8,
  t0: u64,
  t1: u64,
  t2: u64,
  t3: u64,
  t4: u64,
  t5: u64,
  t6: u64,
  t7: u64,
) -> Result<(), String> {
  let zone = zone_from_coords(zone_q, zone_r, zone_z);
  upsert_zone(ctx, zone, definition, t0, t1, t2, t3, t4, t5, t6, t7)
}

#[reducer]
pub fn fill_zone(
  ctx: &ReducerContext,
  zone: u32,
  definition: u8,
  tile_def: u8,
) -> Result<(), String> {
  let row = make_filled_row(tile_def);
  upsert_zone(ctx, zone, definition, row, row, row, row, row, row, row, row)
}

#[reducer]
pub fn fill_zone_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  zone_z: u16,
  definition: u8,
  tile_def: u8,
) -> Result<(), String> {
  let zone = zone_from_coords(zone_q, zone_r, zone_z);
  fill_zone(ctx, zone, definition, tile_def)
}

#[reducer]
pub fn set_zone_definition(
  ctx: &ReducerContext,
  zone: u32,
  definition: u8,
) -> Result<(), String> {
  validate_zone_definition(definition)?;

  let mut row = if let Some(existing) = ctx.db.zones().zone().find(&zone) {
    existing
  } else {
    empty_zone(zone, definition)
  };

  row.definition = definition;

  if ctx.db.zones().zone().find(&zone).is_some() {
    ctx.db.zones().zone().delete(zone);
  }
  ctx.db.zones().insert(row);

  Ok(())
}

#[reducer]
pub fn set_zone_definition_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  zone_z: u16,
  definition: u8,
) -> Result<(), String> {
  let zone = zone_from_coords(zone_q, zone_r, zone_z);
  set_zone_definition(ctx, zone, definition)
}

#[reducer]
pub fn set_zone_row(
  ctx: &ReducerContext,
  zone: u32,
  local_r: u8,
  row_value: u64,
) -> Result<(), String> {
  validate_local_coord("local_r", local_r)?;

  let mut row = if let Some(existing) = ctx.db.zones().zone().find(&zone) {
    existing
  } else {
    empty_zone(zone, 0)
  };

  set_row(&mut row, local_r, row_value);

  if ctx.db.zones().zone().find(&zone).is_some() {
    ctx.db.zones().zone().delete(zone);
  }
  ctx.db.zones().insert(row);

  Ok(())
}

#[reducer]
pub fn set_zone_row_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  zone_z: u16,
  local_r: u8,
  row_value: u64,
) -> Result<(), String> {
  let zone = zone_from_coords(zone_q, zone_r, zone_z);
  set_zone_row(ctx, zone, local_r, row_value)
}

#[reducer]
pub fn set_zone_tile(
  ctx: &ReducerContext,
  zone: u32,
  local_q: u8,
  local_r: u8,
  tile_def: u8,
) -> Result<(), String> {
  validate_local_coord("local_q", local_q)?;
  validate_local_coord("local_r", local_r)?;

  let mut row = if let Some(existing) = ctx.db.zones().zone().find(&zone) {
    existing
  } else {
    empty_zone(zone, 0)
  };

  let current = get_row(&row, local_r);
  let updated = set_packed_tile(current, local_q, tile_def);
  set_row(&mut row, local_r, updated);

  if ctx.db.zones().zone().find(&zone).is_some() {
    ctx.db.zones().zone().delete(zone);
  }
  ctx.db.zones().insert(row);

  Ok(())
}

#[reducer]
pub fn set_zone_tile_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  zone_z: u16,
  local_q: u8,
  local_r: u8,
  tile_def: u8,
) -> Result<(), String> {
  let zone = zone_from_coords(zone_q, zone_r, zone_z);
  set_zone_tile(ctx, zone, local_q, local_r, tile_def)
}

#[reducer]
pub fn delete_zone(
  ctx: &ReducerContext,
  zone: u32,
) -> Result<(), String> {
  if ctx.db.zones().zone().find(&zone).is_some() {
    ctx.db.zones().zone().delete(zone);
  }
  Ok(())
}

#[reducer]
pub fn delete_zone_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  zone_z: u16,
) -> Result<(), String> {
  let zone = zone_from_coords(zone_q, zone_r, zone_z);
  delete_zone(ctx, zone)
}
