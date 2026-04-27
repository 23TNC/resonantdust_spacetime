use spacetimedb::{ReducerContext, Table};
use crate::packing::{pack_macro_world, pack_micro_hex, world_to_zone, world_to_position};

const ACTION_FLAG_STARTED:   u8 = 1 << 0;
const ACTION_FLAG_COMPLETED: u8 = 1 << 1;

#[spacetimedb::table(accessor = actions, public)]
#[derive(Debug, Clone)]
pub struct Action {
  #[primary_key]
  #[auto_inc]
  pub action_id: u32,
  #[index(btree)]
  pub card_id: u32,
  pub recipe: u16,
  pub start: u32,
  pub end: u32,
  pub flags: u8,
  #[index(btree)]
  pub owner_id: u32,
  #[index(btree)]
  pub macro_location: u64,
  pub micro_location: u32,
}

fn current_seconds(ctx: &ReducerContext) -> Result<u32, String> {
  let micros = ctx.timestamp.to_micros_since_unix_epoch();
  if micros < 0 {
    return Err("ReducerContext timestamp is before Unix epoch".to_string());
  }
  let secs = (micros / 1_000_000) as u64;
  u32::try_from(secs).map_err(|_| "ReducerContext timestamp exceeds u32 seconds range".to_string())
}

#[spacetimedb::reducer]
pub fn queue_action(
  ctx: &ReducerContext,
  card_id: u32,
  owner_id: u32,
  recipe: u16,
  q: i32,
  r: i32,
  layer: u8,
) -> Result<(), String> {
  let (zone_q, zone_r) = world_to_zone(q, r);
  let (local_q, local_r) = world_to_position(q, r);
  let now = current_seconds(ctx)?;

  ctx.db.actions().insert(Action {
    action_id: 0,
    card_id,
    recipe,
    start: now,
    end: 0,
    flags: 0,
    owner_id,
    macro_location: pack_macro_world(zone_q, zone_r, layer),
    micro_location: pack_micro_hex(local_q, local_r),
  });

  Ok(())
}

#[spacetimedb::reducer]
pub fn start_action(
  ctx: &ReducerContext,
  action_id: u32,
) -> Result<(), String> {
  let now = current_seconds(ctx)?;
  let expected_end = now
    .checked_add(20)
    .ok_or_else(|| "start time overflow while computing end time".to_string())?;

  if let Some(mut row) = ctx.db.actions().action_id().find(&action_id) {
    row.start = now;
    row.end = expected_end;
    row.flags = ACTION_FLAG_STARTED;
    ctx.db.actions().action_id().update(row);
    Ok(())
  } else {
    Err(format!("action {action_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn complete_action(
  ctx: &ReducerContext,
  action_id: u32,
) -> Result<(), String> {
  let now = current_seconds(ctx)?;

  if let Some(mut row) = ctx.db.actions().action_id().find(&action_id) {
    row.end = now;
    row.flags = ACTION_FLAG_STARTED | ACTION_FLAG_COMPLETED;
    ctx.db.actions().action_id().update(row);
    Ok(())
  } else {
    Err(format!("action {action_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn delete_action(
  ctx: &ReducerContext,
  action_id: u32,
) -> Result<(), String> {
  ctx.db.actions().action_id().delete(&action_id);
  Ok(())
}
