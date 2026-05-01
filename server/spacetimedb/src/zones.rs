use crate::packing::{pack_macro_world, ZONE_SIZE};
use spacetimedb::{reducer, ReducerContext, Table};

#[spacetimedb::table(accessor = zones, public)]
#[derive(Debug, Clone)]
pub struct Zone {
  /// Zone PK component: layer (0..255).  Subscriptions filter by (layer,
  /// macro_zone) — both must equal the queried zone.
  #[index(btree)]
  pub layer: u8,
  /// Zone PK component: world coords packed as [zone_q:i16][zone_r:i16].
  /// Always a world macro_zone for zones; panel layers do not have zone rows.
  #[primary_key]
  pub macro_zone: u32,
  // [ type: u4 ][ category: u4 ]
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
  if (value as i32) >= ZONE_SIZE {
    return Err(format!("{name} must be in 0..={}", ZONE_SIZE - 1));
  }
  Ok(())
}

fn validate_zone_definition(definition: u8) -> Result<(), String> {
  let card_type = definition >> 4;
  let category  = definition & 0x0F;
  if card_type > 0x0F {
    return Err(format!("zone card_type {} exceeds 4 bits", card_type));
  }
  if category > 0x0F {
    return Err(format!("zone category {} exceeds 4 bits", category));
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
  b | (b << 8) | (b << 16) | (b << 24) | (b << 32) | (b << 40) | (b << 48) | (b << 56)
}

fn empty_zone(layer: u8, macro_zone: u32, definition: u8) -> Zone {
  Zone {
    layer,
    macro_zone,
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

#[reducer]
pub fn upsert_zone(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
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

  let row = Zone { layer, macro_zone, definition, t0, t1, t2, t3, t4, t5, t6, t7 };

  if ctx.db.zones().macro_zone().find(&macro_zone).is_some() {
    ctx.db.zones().macro_zone().delete(macro_zone);
  }
  ctx.db.zones().insert(row);

  Ok(())
}

#[reducer]
pub fn upsert_zone_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  layer: u8,
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
  upsert_zone(ctx, layer, pack_macro_world(zone_q, zone_r), definition, t0, t1, t2, t3, t4, t5, t6, t7)
}

#[reducer]
pub fn fill_zone(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
  definition: u8,
  tile_def: u8,
) -> Result<(), String> {
  let row = make_filled_row(tile_def);
  upsert_zone(ctx, layer, macro_zone, definition, row, row, row, row, row, row, row, row)
}

#[reducer]
pub fn fill_zone_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  layer: u8,
  definition: u8,
  tile_def: u8,
) -> Result<(), String> {
  fill_zone(ctx, layer, pack_macro_world(zone_q, zone_r), definition, tile_def)
}

#[reducer]
pub fn set_zone_definition(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
  definition: u8,
) -> Result<(), String> {
  validate_zone_definition(definition)?;

  let mut row = ctx.db.zones().macro_zone().find(&macro_zone)
    .unwrap_or_else(|| empty_zone(layer, macro_zone, definition));

  row.definition = definition;
  row.layer      = layer;

  if ctx.db.zones().macro_zone().find(&macro_zone).is_some() {
    ctx.db.zones().macro_zone().delete(macro_zone);
  }
  ctx.db.zones().insert(row);

  Ok(())
}

#[reducer]
pub fn set_zone_definition_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  layer: u8,
  definition: u8,
) -> Result<(), String> {
  set_zone_definition(ctx, layer, pack_macro_world(zone_q, zone_r), definition)
}

#[reducer]
pub fn set_zone_row(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
  local_r: u8,
  row_value: u64,
) -> Result<(), String> {
  validate_local_coord("local_r", local_r)?;

  let mut row = ctx.db.zones().macro_zone().find(&macro_zone)
    .unwrap_or_else(|| empty_zone(layer, macro_zone, 0));

  set_row(&mut row, local_r, row_value);

  if ctx.db.zones().macro_zone().find(&macro_zone).is_some() {
    ctx.db.zones().macro_zone().delete(macro_zone);
  }
  ctx.db.zones().insert(row);

  Ok(())
}

#[reducer]
pub fn set_zone_row_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  layer: u8,
  local_r: u8,
  row_value: u64,
) -> Result<(), String> {
  set_zone_row(ctx, layer, pack_macro_world(zone_q, zone_r), local_r, row_value)
}

#[reducer]
pub fn set_zone_tile(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
  local_q: u8,
  local_r: u8,
  tile_def: u8,
) -> Result<(), String> {
  validate_local_coord("local_q", local_q)?;
  validate_local_coord("local_r", local_r)?;

  let mut row = ctx.db.zones().macro_zone().find(&macro_zone)
    .unwrap_or_else(|| empty_zone(layer, macro_zone, 0));

  let current = get_row(&row, local_r);
  let updated = set_packed_tile(current, local_q, tile_def);
  set_row(&mut row, local_r, updated);

  if ctx.db.zones().macro_zone().find(&macro_zone).is_some() {
    ctx.db.zones().macro_zone().delete(macro_zone);
  }
  ctx.db.zones().insert(row);

  Ok(())
}

#[reducer]
pub fn set_zone_tile_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  layer: u8,
  local_q: u8,
  local_r: u8,
  tile_def: u8,
) -> Result<(), String> {
  set_zone_tile(ctx, layer, pack_macro_world(zone_q, zone_r), local_q, local_r, tile_def)
}

#[reducer]
pub fn delete_zone(
  ctx: &ReducerContext,
  macro_zone: u32,
) -> Result<(), String> {
  ctx.db.zones().macro_zone().delete(macro_zone);
  Ok(())
}

#[reducer]
pub fn delete_zone_at(
  ctx: &ReducerContext,
  zone_q: i16,
  zone_r: i16,
  _layer: u8,
) -> Result<(), String> {
  delete_zone(ctx, pack_macro_world(zone_q, zone_r))
}
