use spacetimedb::{reducer, ReducerContext, Table};
use crate::cards::{cards, insert_card_row};
use crate::packing::{pack_macro_world, pack_micro_hex, world_to_zone, world_to_position};

#[spacetimedb::table(accessor = players, public)]
#[derive(Debug, Clone)]
pub struct Player {
  #[primary_key]
  #[auto_inc]
  pub player_id: u32,
  #[unique]
  pub name: String,
  #[index(btree)]
  pub soul_id: u32,
  #[index(btree)]
  pub macro_location: u64,
  pub micro_location: u32,
}

#[reducer]
pub fn upsert_player(
  ctx: &ReducerContext,
  name: String,
  card_type: u8,
  category: u8,
  definition_id: u8,
  flags: u16,
  q: i32,
  r: i32,
  layer: u8,
) -> Result<(), String> {
  if ctx.db.players().name().find(&name).is_some() {
    return Ok(());
  }

  let soul_card_id = insert_card_row(ctx, card_type, category, definition_id, 0, flags, q, r, layer)?;

  let (macro_location, micro_location) =
    if let Some(mut soul_card) = ctx.db.cards().card_id().find(&soul_card_id) {
      soul_card.owner_id = soul_card_id;
      let loc = (soul_card.macro_location, soul_card.micro_location);
      ctx.db.cards().card_id().update(soul_card);
      loc
    } else {
      return Err(format!("soul card {soul_card_id} was not found after insert"));
    };

  ctx.db.players().try_insert(Player {
    player_id: 0,
    name,
    soul_id: soul_card_id,
    macro_location,
    micro_location,
  })?;

  Ok(())
}

#[reducer]
pub fn update_player_soul_id(
  ctx: &ReducerContext,
  player_id: u32,
  soul_id: u32,
) -> Result<(), String> {
  if let Some(mut row) = ctx.db.players().player_id().find(&player_id) {
    row.soul_id = soul_id;

    if let Some(soul_card) = ctx.db.cards().card_id().find(&soul_id) {
      row.macro_location = soul_card.macro_location;
      row.micro_location = soul_card.micro_location;
    }

    ctx.db.players().player_id().update(row);
    Ok(())
  } else {
    Err(format!("player {player_id} not found"))
  }
}

#[reducer]
pub fn update_player_location(
  ctx: &ReducerContext,
  player_id: u32,
  q: i32,
  r: i32,
  layer: u8,
) -> Result<(), String> {
  let (zone_q, zone_r) = world_to_zone(q, r);
  let (local_q, local_r) = world_to_position(q, r);

  if let Some(mut row) = ctx.db.players().player_id().find(&player_id) {
    row.macro_location = pack_macro_world(zone_q, zone_r, layer);
    row.micro_location = pack_micro_hex(local_q, local_r);
    ctx.db.players().player_id().update(row);
    Ok(())
  } else {
    Err(format!("player {player_id} not found"))
  }
}

#[reducer]
pub fn delete_player(
  ctx: &ReducerContext,
  player_id: u32,
) -> Result<(), String> {
  ctx.db.players().player_id().delete(&player_id);
  Ok(())
}
