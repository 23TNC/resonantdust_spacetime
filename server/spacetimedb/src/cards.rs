use spacetimedb::{ReducerContext, Table};
use crate::packing::{
  pack_definition, pack_macro_world, pack_macro_panel, pack_micro_hex, pack_micro_pixel, pack_micro_stacked,
  card_type_from_definition, definition_id_from_definition, world_to_zone, world_to_position,
  CARD_FLAG_STACKED_UP, CARD_FLAG_STACKED_DOWN, CARD_FLAG_STACKABLE,
};
use crate::actions::{actions, start_action_inner, start_action_inner_pool, delete_action_rows};
use crate::players::players;

#[spacetimedb::table(accessor = cards, public)]
#[derive(Debug, Clone)]
pub struct Card {
  #[primary_key]
  #[auto_inc]
  pub card_id: u32,
  // [ zone_q: i16 ][ zone_r: i16 ][ reserved: u16 ][ layer: u8 ][ surface: u8 ]
  #[index(btree)]
  pub macro_location: u64,
  // stacked: stacked_id (u32) | hex: [ local_q: u4 ][ local_r: u4 ][ reserved: u24 ] | pixel: [ x: i16 ][ y: i16 ]
  pub micro_location: u32,
  #[index(btree)]
  pub owner_id: u32,
  // [ stacked: u1 ][ stackable: u1 ][ position_locked: u1 ][ position_hold: u1 ][ reserved: u12 ]
  pub flags: u16,
  // [ card_type: u4 ][ category: u4 ][ definition_id: u8 ]
  pub packed_definition: u16,
  // Card-type-specific payload. Soul Reference (type 4 / def 1): data[31:0] holds the world soul card_id.
  pub data:      u64,
  pub action_id: u64,
}

fn sync_player_location_for_soul_card(ctx: &ReducerContext, card: &Card) {
  if card_type_from_definition(card.packed_definition) != 5 {
    return;
  }
  for mut player in ctx.db.players().soul_id().filter(&card.card_id) {
    player.macro_location = card.macro_location;
    player.micro_location = card.micro_location;
    ctx.db.players().player_id().update(player);
  }
}

fn start_on_create_action(ctx: &ReducerContext, card: &Card) -> Result<(), String> {
  let card_type     = card_type_from_definition(card.packed_definition);
  let definition_id = definition_id_from_definition(card.packed_definition);
  let Some(def) = crate::definitions::get_card_def(card_type, definition_id) else {
    return Ok(());
  };
  // Pool: def id (qty 1) + each aspect at its value.
  // Recipes can match by exact card id, aspect name, or "any".
  let mut pool: std::collections::HashMap<String, u32> = def.aspects
    .iter()
    .map(|(k, &v)| (k.clone(), v as u32))
    .collect();
  *pool.entry(def.id.clone()).or_insert(0) += 1;

  // Collect all matching on_create recipes and pick the most specific one.
  // Specificity is scored per definitions::score_recipe_for_card:
  //   def id match > aspect match > card_type match > "any" match.
  let mut best: Option<(&'static crate::definitions::RecipeDef, u32)> = None;
  for recipe in crate::definitions::on_create_recipes() {
    let mut p = pool.clone();
    if crate::definitions::matches_inputs(recipe, &mut p) {
      let score = crate::definitions::score_recipe_for_card(recipe, def);
      if best.map_or(true, |(_, s)| score > s) {
        best = Some((recipe, score));
      }
    }
  }

  if let Some((recipe, _)) = best {
    return start_action_inner_pool(
      ctx, card.card_id, card.owner_id, recipe.index,
      card.macro_location, card.micro_location, &pool,
    );
  }
  Ok(())
}

pub fn insert_card_row(
  ctx: &ReducerContext,
  card_type: u8,
  category: u8,
  definition_id: u8,
  owner_id: u32,
  flags: u16,
  q: i32,
  r: i32,
  layer: u8,
) -> Result<u32, String> {
  let (zone_q, zone_r) = world_to_zone(q, r);
  let (local_q, local_r) = world_to_position(q, r);

  let inserted = ctx.db.cards().insert(Card {
    card_id: 0,
    macro_location: pack_macro_world(zone_q, zone_r, layer),
    micro_location: pack_micro_hex(local_q, local_r),
    owner_id,
    flags,
    packed_definition: pack_definition(card_type, category, definition_id),
    data: 0,
    action_id: 0,
  });

  // A type-5 card with category > 0 is an instance of a soul archetype.
  // Create a companion Soul Reference (Revery, type 4 / category 0 / def 1)
  // in the soul's panel (surface=2).  data[31:0] holds the world soul
  // card_id via the card_target ability so the reference card can resolve
  // back to its soul.
  if card_type == 5 && category > 0 {
    ctx.db.cards().insert(Card {
      card_id: 0,
      macro_location: pack_macro_panel(inserted.card_id, 1),
      micro_location: 0,
      owner_id,
      flags: CARD_FLAG_STACKABLE,
      packed_definition: pack_definition(4, 0, 1),
      data: inserted.card_id as u64,
      action_id: 0,
    });
  }

  start_on_create_action(ctx, &inserted)?;
  Ok(inserted.card_id)
}

pub fn insert_panel_card_row(
  ctx: &ReducerContext,
  card_type: u8,
  category: u8,
  definition_id: u8,
  owner_id: u32,
  flags: u16,
) -> Result<u32, String> {
  let inserted = ctx.db.cards().insert(Card {
    card_id:           0,
    macro_location:    pack_macro_panel(owner_id, 1),
    micro_location:    pack_micro_pixel(0, 0),
    owner_id,
    flags,
    packed_definition: pack_definition(card_type, category, definition_id),
    data:              0,
    action_id:         0,
  });

  start_on_create_action(ctx, &inserted)?;
  Ok(inserted.card_id)
}

#[spacetimedb::reducer]
pub fn update_card_owner_id(
  ctx: &ReducerContext,
  card_id: u32,
  owner_id: u32,
) -> Result<(), String> {
  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.owner_id = owner_id;
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn update_card_flags(
  ctx: &ReducerContext,
  card_id: u32,
  flags: u16,
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
pub fn update_card_data(
  ctx: &ReducerContext,
  card_id: u32,
  data: u64,
) -> Result<(), String> {
  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.data = data;
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn update_card_action_id(
  ctx: &ReducerContext,
  card_id: u32,
  action_id: u64,
) -> Result<(), String> {
  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.action_id = action_id;
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
  layer: u8,
) -> Result<(), String> {
  let (zone_q, zone_r) = world_to_zone(q, r);
  let (local_q, local_r) = world_to_position(q, r);
  let macro_location = pack_macro_world(zone_q, zone_r, layer);
  let micro_location = pack_micro_hex(local_q, local_r);

  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.macro_location = macro_location;
    row.micro_location = micro_location;
    ctx.db.cards().card_id().update(row.clone());
    sync_player_location_for_soul_card(ctx, &row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn update_card_micro_location(
  ctx: &ReducerContext,
  card_id: u32,
  micro_location: u32,
) -> Result<(), String> {
  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.micro_location = micro_location;
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn stack_card_up(
  ctx: &ReducerContext,
  card_id: u32,
  onto_id: u32,
) -> Result<(), String> {
  let onto = ctx.db.cards().card_id().find(&onto_id)
    .ok_or_else(|| format!("card {onto_id} not found"))?;

  if onto.flags & CARD_FLAG_STACKED_DOWN != 0 {
    return Err(format!("cannot stack_up onto card {onto_id}: it has STACKED_DOWN set"));
  }

  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.macro_location = onto.macro_location;
    row.micro_location = pack_micro_stacked(onto_id);
    row.flags &= !(CARD_FLAG_STACKED_UP | CARD_FLAG_STACKED_DOWN);
    row.flags |= CARD_FLAG_STACKED_UP;
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn stack_card_down(
  ctx: &ReducerContext,
  card_id: u32,
  onto_id: u32,
) -> Result<(), String> {
  let onto = ctx.db.cards().card_id().find(&onto_id)
    .ok_or_else(|| format!("card {onto_id} not found"))?;

  if onto.flags & CARD_FLAG_STACKED_UP != 0 {
    return Err(format!("cannot stack_down onto card {onto_id}: it has STACKED_UP set"));
  }

  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.macro_location = onto.macro_location;
    row.micro_location = pack_micro_stacked(onto_id);
    row.flags &= !(CARD_FLAG_STACKED_UP | CARD_FLAG_STACKED_DOWN);
    row.flags |= CARD_FLAG_STACKED_DOWN;
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

#[spacetimedb::reducer]
pub fn unstack_card(
  ctx: &ReducerContext,
  card_id: u32,
  q: i32,
  r: i32,
  layer: u8,
) -> Result<(), String> {
  let (zone_q, zone_r) = world_to_zone(q, r);
  let (local_q, local_r) = world_to_position(q, r);

  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.macro_location = pack_macro_world(zone_q, zone_r, layer);
    row.micro_location = pack_micro_hex(local_q, local_r);
    row.flags &= !(CARD_FLAG_STACKED_UP | CARD_FLAG_STACKED_DOWN);
    ctx.db.cards().card_id().update(row);
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
  for action in ctx.db.actions().card_id().filter(&card_id) {
    delete_action_rows(ctx, action.action_id);
  }
  ctx.db.cards().card_id().delete(&card_id);
  Ok(())
}
