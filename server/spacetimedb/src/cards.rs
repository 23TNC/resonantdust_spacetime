use spacetimedb::{ReducerContext, Table};
use crate::packing::{pack_position, pack_zone, world_to_position, world_to_zone};
use crate::players::players;

#[spacetimedb::table(accessor = cards, public)]
#[derive(Debug, Clone)]
pub struct Card {
  #[primary_key]
  #[auto_inc]
  pub card_id: u32,
  pub definition: u16,
  #[index(btree)]
  pub soul_id: u32,
  #[index(btree)]
  pub link_id: u32,
  pub flags: u64,
  #[index(btree)]
  pub zone: u32,
  pub position: u8,
}

fn pack_definition(card_type: u8, definition_id: u16) -> Result<u16, String> {
  if card_type > 0x0F {
    return Err(format!("card_type {} exceeds 4 bits", card_type));
  }
  if definition_id > 0x0FFF {
    return Err(format!("definition_id {} exceeds 12 bits", definition_id));
  }

  Ok(((card_type as u16) << 12) | definition_id)
}

fn card_type_from_definition(definition: u16) -> u8 {
  ((definition >> 12) & 0x0f) as u8
}

fn sync_player_location_for_soul_card(
  ctx: &ReducerContext,
  card: &Card,
) {
  if card_type_from_definition(card.definition) != 5 {
    return;
  }

  for mut player in ctx.db.players().soul_id().filter(&card.card_id) {
    player.zone = card.zone;
    player.position = card.position;
    ctx.db.players().player_id().update(player);
  }
}

pub fn insert_card_row(
  ctx: &ReducerContext,
  card_type: u8,
  definition_id: u16,
  soul_id: u32,
  link_id: u32,
  flags: u64,
  q: i32,
  r: i32,
  z: u16,
) -> Result<u32, String> {
  let definition = pack_definition(card_type, definition_id)?;

  let (zone_q, zone_r) = world_to_zone(q, r);
  let (pos_q, pos_r) = world_to_position(q, r);

  let zone = pack_zone(zone_q, zone_r, z);
  let position = pack_position(pos_q, pos_r);

  let inserted = ctx.db.cards().insert(Card {
    card_id: 0,
    definition,
    soul_id,
    link_id,
    flags,
    zone,
    position,
  });

  Ok(inserted.card_id)
}

pub fn insert_card(
  ctx: &ReducerContext,
  card_type: u8,
  definition_id: u16,
  soul_id: u32,
  link_id: u32,
  flags: u64,
  q: i32,
  r: i32,
  z: u16,
) -> Result<(), String> {
  insert_card_row(ctx, card_type, definition_id, soul_id, link_id, flags, q, r, z)?;
  Ok(())
}

#[spacetimedb::reducer]
pub fn update_card_soul_id(
  ctx: &ReducerContext,
  card_id: u32,
  soul_id: u32,
) -> Result<(), String> {
  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.soul_id = soul_id;
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn update_card_link_id(
  ctx: &ReducerContext,
  card_id: u32,
  link_id: u32,
) -> Result<(), String> {
  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.link_id = link_id;
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn set_card_flags(
  ctx: &ReducerContext,
  card_id: u32,
  flags: u64,
) -> Result<(), String> {
  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.flags = flags;
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn update_card_location(
  ctx: &ReducerContext,
  card_id: u32,
  q: i32,
  r: i32,
  z: u16,
) -> Result<(), String> {
  let (zone_q, zone_r) = world_to_zone(q, r);
  let (pos_q, pos_r) = world_to_position(q, r);

  let zone = pack_zone(zone_q, zone_r, z);
  let position = pack_position(pos_q, pos_r);

  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.zone = zone;
    row.position = position;
    ctx.db.cards().card_id().update(row.clone());
    sync_player_location_for_soul_card(ctx, &row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn delete_card(
  ctx: &ReducerContext,
  card_id: u32,
) -> Result<(), String> {
  ctx.db.cards().card_id().delete(&card_id);
  Ok(())
}
