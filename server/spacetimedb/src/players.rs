use spacetimedb::{reducer, ReducerContext, Table};
use crate::cards::{cards, insert_card_row};
use crate::packing::{pack_macro_world, pack_micro_zone, world_to_zone, world_to_position};

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
  /// Layer the player's soul currently occupies (always a world layer for
  /// the soul card itself; players don't sit "in panel layers").
  pub layer: u8,
  /// World macro_zone the soul currently occupies.
  #[index(btree)]
  pub macro_zone: u32,
  /// In-zone hex position of the soul.
  pub micro_zone: u8,
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

  let (player_layer, macro_zone, micro_zone) =
    if let Some(mut soul_card) = ctx.db.cards().card_id().find(&soul_card_id) {
      soul_card.owner_id = soul_card_id;
      let loc = (soul_card.layer, soul_card.macro_zone, soul_card.micro_zone);
      ctx.db.cards().card_id().update(soul_card);
      loc
    } else {
      return Err(format!("soul card {soul_card_id} was not found after insert"));
    };

  ctx.db.players().try_insert(Player {
    player_id: 0,
    name,
    soul_id:    soul_card_id,
    layer:      player_layer,
    macro_zone,
    micro_zone,
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
      row.layer      = soul_card.layer;
      row.macro_zone = soul_card.macro_zone;
      row.micro_zone = soul_card.micro_zone;
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
    row.layer      = layer;
    row.macro_zone = pack_macro_world(zone_q, zone_r);
    row.micro_zone = pack_micro_zone(local_q, local_r);
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
