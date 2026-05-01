use spacetimedb::{ReducerContext, Table};
use crate::packing::{
  pack_definition, pack_macro_world, pack_macro_panel,
  pack_micro_zone, pack_micro_pixel, pack_micro_parent, pack_micro_attached,
  card_type_from_definition, definition_id_from_definition,
  world_to_zone, world_to_position,
  with_stack_state, stack_state, is_stacked,
  STACK_STATE_LOOSE, STACK_STATE_UP, STACK_STATE_DOWN, STACK_STATE_ATTACHED,
  PANEL_LAYER_INVENTORY, MICRO_ATTACHED_TO_FLOOR,
  CARD_FLAG_STACKABLE,
};
use crate::actions::{actions, start_action_inner_pool, delete_action_rows};
use crate::players::players;

#[spacetimedb::table(accessor = cards, public)]
#[derive(Debug, Clone)]
pub struct Card {
  #[primary_key]
  #[auto_inc]
  pub card_id: u32,
  /// Subscription discriminator: panel layers (0..32) hold a soul_card_id in
  /// `macro_zone`; world layers (32..255) hold packed (zone_q, zone_r).
  #[index(btree)]
  pub layer: u8,
  /// Either a soul_card_id (panel layer) or packed [zone_q:i16][zone_r:i16]
  /// (world layer).  Stacked / attached cards mirror their anchor here so
  /// subscriptions return the chain alongside the anchor.
  #[index(btree)]
  pub macro_zone: u32,
  /// In-zone hex coords [local_q:u3][local_r:u3][unused:u2].  For panel cards
  /// or stacked/attached cards, mirrors anchor.
  pub micro_zone: u8,
  /// Variant per stack_state (in `flags`):
  ///   00 loose:    [pixel_x:i16][pixel_y:i16]
  ///   01 up:       parent rect card_id
  ///   10 down:     parent rect card_id
  ///   11 attached: hex card_id, or 0 = "floor at my (macro_zone, micro_zone)"
  pub micro_location: u32,
  #[index(btree)]
  pub owner_id: u32,
  /// See `packing::CARD_FLAG_*`.  STACK_STATE lives in bits 6-7.
  pub flags: u16,
  /// [card_type:u4][category:u4][definition_id:u8]
  pub packed_definition: u16,
  /// Card-type-specific payload. Soul Reference (Revery def 1): low 32 bits
  /// hold the world soul card_id.
  pub data: u64,
  pub action_id: u64,
}

fn sync_player_location_for_soul_card(ctx: &ReducerContext, card: &Card) {
  sync_player_location_for_soul_card_pub(ctx, card);
}

/// Cross-module accessor so `actions::update_position` can keep `Player`
/// rows in sync with their soul card after a position update.  Same body
/// as the private wrapper used by the Card reducers in this module.
pub fn sync_player_location_for_soul_card_pub(ctx: &ReducerContext, card: &Card) {
  if card_type_from_definition(card.packed_definition) != crate::definitions::card_types().soul {
    return;
  }
  for mut player in ctx.db.players().soul_id().filter(&card.card_id) {
    player.layer      = card.layer;
    player.macro_zone = card.macro_zone;
    player.micro_zone = card.micro_zone;
    ctx.db.players().player_id().update(player);
  }
}

fn start_on_create_action(ctx: &ReducerContext, card: &Card) -> Result<(), String> {
  let card_type     = card_type_from_definition(card.packed_definition);
  let definition_id = definition_id_from_definition(card.packed_definition);
  let Some(def) = crate::definitions::get_card_def(card_type, definition_id) else {
    return Ok(());
  };

  // For on_create, the chain is just [card] and actor_index is 0.
  let chain_tuples: Vec<(u32, &'static crate::definitions::CardDef, u16)> =
    vec![(card.card_id, def, card.flags)];

  let mut best: Option<(&'static crate::definitions::RecipeDef, u32)> = None;
  for recipe in crate::definitions::on_create_recipes() {
    let actor_index = crate::definitions::actor_index_for(recipe);
    if let Some(result) = crate::definitions::try_match_recipe_at(recipe, &chain_tuples, actor_index) {
      if best.map_or(true, |(_, s)| result.weight > s) {
        best = Some((recipe, result.weight));
      }
    }
  }

  if let Some((recipe, _)) = best {
    let mut pool: std::collections::HashMap<String, u32> = def.aspects
      .iter()
      .map(|(k, &v)| (k.clone(), v as u32))
      .collect();
    *pool.entry(def.id.clone()).or_insert(0) += 1;
    return start_action_inner_pool(
      ctx, card.card_id, card.owner_id, recipe.index,
      card.layer, card.macro_zone, card.micro_zone, &pool,
    );
  }
  Ok(())
}

/// Insert a world-positioned card row at world coords (q, r) on `layer`.
/// Used by bootstrap and tests.
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

  // World cards default to stack_state == 00 (loose, hex-positioned).
  let inserted = ctx.db.cards().insert(Card {
    card_id: 0,
    layer,
    macro_zone:    pack_macro_world(zone_q, zone_r),
    micro_zone:    pack_micro_zone(local_q, local_r),
    micro_location: pack_micro_pixel(0, 0),
    owner_id,
    flags: with_stack_state(flags, STACK_STATE_LOOSE),
    packed_definition: pack_definition(card_type, category, definition_id),
    data: 0,
    action_id: 0,
  });

  // A Soul card with category > 0 is an instance of a soul archetype.
  // Spawn a companion Soul Reference (Revery def 1) into the soul's panel
  // (panel layer / macro_zone = soul_card_id).  The reference card holds
  // the world soul card_id in `data` for resolution.
  let types = crate::definitions::card_types();
  if card_type == types.soul && category > 0 {
    ctx.db.cards().insert(Card {
      card_id: 0,
      layer:          PANEL_LAYER_INVENTORY,
      macro_zone:     pack_macro_panel(inserted.card_id),
      micro_zone:     0,
      micro_location: pack_micro_pixel(0, 0),
      owner_id,
      flags: with_stack_state(CARD_FLAG_STACKABLE, STACK_STATE_LOOSE),
      packed_definition: pack_definition(types.revery, 0, 1),
      data: inserted.card_id as u64,
      action_id: 0,
    });
  }

  start_on_create_action(ctx, &inserted)?;
  Ok(inserted.card_id)
}

/// Insert a panel-positioned card row in the inventory of `owner_id`.
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
    layer:             PANEL_LAYER_INVENTORY,
    macro_zone:        pack_macro_panel(owner_id),
    micro_zone:        0,
    micro_location:    pack_micro_pixel(0, 0),
    owner_id,
    flags: with_stack_state(flags, STACK_STATE_LOOSE),
    packed_definition: pack_definition(card_type, category, definition_id),
    data:              0,
    action_id:         0,
  });

  start_on_create_action(ctx, &inserted)?;
  Ok(inserted.card_id)
}

/// Insert a card row at a precomputed world position.  Used by world-
/// placement product targets, which already have `(layer, macro_zone,
/// micro_zone)` resolved from the recipe's destination rule.  The card
/// lands as a LOOSE root with a (0, 0) cosmetic pixel offset; future
/// sub-hex placement rules can vary the offset.
pub fn insert_world_card_row_at(
  ctx:           &ReducerContext,
  card_type:     u8,
  category:      u8,
  definition_id: u8,
  owner_id:      u32,
  flags:         u16,
  layer:         u8,
  macro_zone:    u32,
  micro_zone:    u8,
) -> Result<u32, String> {
  let inserted = ctx.db.cards().insert(Card {
    card_id:           0,
    layer,
    macro_zone,
    micro_zone,
    micro_location:    pack_micro_pixel(0, 0),
    owner_id,
    flags: with_stack_state(flags, STACK_STATE_LOOSE),
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

/// Place a card at a world hex (q, r) on `layer`, as a loose root.
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

  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.layer      = layer;
    row.macro_zone = pack_macro_world(zone_q, zone_r);
    row.micro_zone = pack_micro_zone(local_q, local_r);
    row.flags      = with_stack_state(row.flags, STACK_STATE_LOOSE);
    // Reset micro_location to a default cosmetic offset; callers wanting a
    // specific pixel offset should route through `update_position`.
    row.micro_location = pack_micro_pixel(0, 0);
    ctx.db.cards().card_id().update(row.clone());
    sync_player_location_for_soul_card(ctx, &row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

/// Update only the cosmetic pixel offset of a loose-root card.  The card
/// must already be in stack_state == LOOSE.
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

/// Stack `card_id` upward onto `onto_id` (the new card sits above the parent).
/// Mirrors the parent's (layer, macro_zone, micro_zone); sets micro_location
/// to the parent id; sets STACK_STATE to UP.
#[spacetimedb::reducer]
pub fn stack_card_up(
  ctx: &ReducerContext,
  card_id: u32,
  onto_id: u32,
) -> Result<(), String> {
  let onto = ctx.db.cards().card_id().find(&onto_id)
    .ok_or_else(|| format!("card {onto_id} not found"))?;

  if stack_state(onto.flags) == STACK_STATE_DOWN {
    return Err(format!("cannot stack_up onto card {onto_id}: it is STACKED_DOWN"));
  }

  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.layer          = onto.layer;
    row.macro_zone     = onto.macro_zone;
    row.micro_zone     = onto.micro_zone;
    row.micro_location = pack_micro_parent(onto_id);
    row.flags          = with_stack_state(row.flags, STACK_STATE_UP);
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

  if stack_state(onto.flags) == STACK_STATE_UP {
    return Err(format!("cannot stack_down onto card {onto_id}: it is STACKED_UP"));
  }

  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.layer          = onto.layer;
    row.macro_zone     = onto.macro_zone;
    row.micro_zone     = onto.micro_zone;
    row.micro_location = pack_micro_parent(onto_id);
    row.flags          = with_stack_state(row.flags, STACK_STATE_DOWN);
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

/// Detach a card from its parent and place it at world hex (q, r) on `layer`
/// as a loose root.  micro_location is reset to a (0, 0) pixel offset.
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
    row.layer          = layer;
    row.macro_zone     = pack_macro_world(zone_q, zone_r);
    row.micro_zone     = pack_micro_zone(local_q, local_r);
    row.micro_location = pack_micro_pixel(0, 0);
    row.flags          = with_stack_state(row.flags, STACK_STATE_LOOSE);
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

/// Attach a card as a root to a hex card.  The new card mirrors the hex
/// card's (layer, macro_zone, micro_zone); sets micro_location to the hex
/// card_id (or 0 to mean "floor at my own hex" — see `attach_card_to_floor`).
#[spacetimedb::reducer]
pub fn attach_card_to_hex(
  ctx: &ReducerContext,
  card_id: u32,
  hex_card_id: u32,
) -> Result<(), String> {
  let hex = ctx.db.cards().card_id().find(&hex_card_id)
    .ok_or_else(|| format!("hex card {hex_card_id} not found"))?;

  let hex_type = card_type_from_definition(hex.packed_definition);
  if !crate::packing::is_hex_card(hex_type) {
    return Err(format!("card {hex_card_id} is not a hex card (card_type {hex_type})"));
  }

  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.layer          = hex.layer;
    row.macro_zone     = hex.macro_zone;
    row.micro_zone     = hex.micro_zone;
    row.micro_location = pack_micro_attached(hex_card_id);
    row.flags          = with_stack_state(row.flags, STACK_STATE_ATTACHED);
    ctx.db.cards().card_id().update(row);
    Ok(())
  } else {
    Err(format!("card {card_id} not found"))
  }
}

/// Attach a card as a root to whatever hex card sits at world (q, r) on
/// `layer` — usually a zone-derived floor card.  Resolution happens at
/// recipe-evaluation time.
#[spacetimedb::reducer]
pub fn attach_card_to_floor(
  ctx: &ReducerContext,
  card_id: u32,
  q: i32,
  r: i32,
  layer: u8,
) -> Result<(), String> {
  let (zone_q, zone_r) = world_to_zone(q, r);
  let (local_q, local_r) = world_to_position(q, r);

  if let Some(mut row) = ctx.db.cards().card_id().find(&card_id) {
    row.layer          = layer;
    row.macro_zone     = pack_macro_world(zone_q, zone_r);
    row.micro_zone     = pack_micro_zone(local_q, local_r);
    row.micro_location = MICRO_ATTACHED_TO_FLOOR;
    row.flags          = with_stack_state(row.flags, STACK_STATE_ATTACHED);
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

// ─── Internal helpers used by other modules ──────────────────────────────────

/// Read the parent card_id for a stacked card.  Returns None if the card is
/// not in a stacked stack_state.
#[allow(dead_code)]
pub fn parent_id_of(card: &Card) -> Option<u32> {
  if is_stacked(card.flags) { Some(card.micro_location) } else { None }
}
