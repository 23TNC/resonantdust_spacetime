use spacetimedb::{reducer, ReducerContext, Table};
use crate::cards::{cards, insert_card_row};

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
  pub zone: u32,
  pub position: u8,
}

#[reducer]
pub fn upsert_player(
  ctx: &ReducerContext,
  name: String,
  card_type: u8,
  definition_id: u16,
  flags: u64,
  q: i32,
  r: i32,
  z: u16,
) -> Result<(), String> {
  if ctx.db.players().name().find(&name).is_some() {
    return Ok(());
  }

  let soul_card_id = insert_card_row(
    ctx,
    card_type,
    definition_id,
    0,
    0,
    flags,
    q,
    r,
    z,
  )?;

  let (zone, position) = if let Some(mut soul_card) = ctx.db.cards().card_id().find(&soul_card_id) {
    soul_card.soul_id = soul_card_id;

    let zone = soul_card.zone;
    let position = soul_card.position;

    ctx.db.cards().card_id().update(soul_card);

    (zone, position)
  } else {
    return Err(format!("soul card {soul_card_id} was not found after insert"));
  };

  ctx.db.players().try_insert(Player {
    player_id: 0,
    name,
    soul_id: soul_card_id,
    zone,
    position,
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
      row.zone = soul_card.zone;
      row.position = soul_card.position;
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
  zone: u32,
  position: u8,
) -> Result<(), String> {
  if let Some(mut row) = ctx.db.players().player_id().find(&player_id) {
    row.zone = zone;
    row.position = position;
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
