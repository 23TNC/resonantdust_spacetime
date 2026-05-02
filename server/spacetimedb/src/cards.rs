use spacetimedb::{ReducerContext, Table};
use crate::packing::{
  pack_definition, pack_macro_world, pack_macro_panel,
  pack_micro_zone, pack_micro_pixel,
  card_type_from_definition, definition_id_from_definition,
  world_to_zone, world_to_position,
  with_stack_state,
  STACK_STATE_LOOSE,
  PANEL_LAYER_INVENTORY,
  CARD_FLAG_STACKABLE,
};
use crate::actions::{actions, card_holds, start_action_inner_pool, delete_action_rows};
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
  /// (world layer).  Stacked cards mirror their root's anchor here so
  /// subscriptions return the chain alongside the root.
  #[index(btree)]
  pub macro_zone: u32,
  /// In-zone hex coords [local_q:u3][local_r:u3][unused:u2].  For panel
  /// or stacked cards, mirrors anchor.
  pub micro_zone: u8,
  /// Variant per stack_state (in `flags`):
  ///   00 loose: [pixel_x:i16][pixel_y:i16]
  ///   01 up:    parent rect card_id
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
}

/// Cross-module accessor so `actions::update_position` can keep `Player`
/// rows in sync with their soul card after a position update.
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

  // For on_create, the chain is just [card] and actor_index is 0.  A
  // freshly-inserted card cannot already be held, so an empty held set
  // is correct here.
  let chain_tuples: Vec<(u32, &'static crate::definitions::CardDef)> =
    vec![(card.card_id, def)];
  let held: std::collections::HashSet<u32> = std::collections::HashSet::new();

  let mut best: Option<(&'static crate::definitions::RecipeDef, u32)> = None;
  for recipe in crate::definitions::on_create_recipes() {
    let actor_index = crate::definitions::actor_index_for(recipe);
    if let Some(result) = crate::definitions::try_match_recipe_at(recipe, &chain_tuples, actor_index, &held) {
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
    layer,
    macro_zone:    pack_macro_world(zone_q, zone_r),
    micro_zone:    pack_micro_zone(local_q, local_r),
    micro_location: pack_micro_pixel(0, 0),
    owner_id,
    flags: with_stack_state(flags, STACK_STATE_LOOSE),
    packed_definition: pack_definition(card_type, category, definition_id),
    data: 0,
  });

  // A Soul card with category > 0 is an instance of a soul archetype.
  // Spawn a companion Soul Reference (Revery def 1) into the soul's panel.
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
  });

  start_on_create_action(ctx, &inserted)?;
  Ok(inserted.card_id)
}

/// Insert a card row at a precomputed world position.  Used by recipe
/// product targets that resolve directly to (layer, macro_zone, micro_zone).
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
  });

  start_on_create_action(ctx, &inserted)?;
  Ok(inserted.card_id)
}

#[spacetimedb::reducer]
pub fn delete_card(
  ctx: &ReducerContext,
  card_id: u32,
) -> Result<(), String> {
  // Cancel any action whose ACTOR is this card.
  for action in ctx.db.actions().card_id().filter(&card_id) {
    delete_action_rows(ctx, action.action_id);
  }
  // Cancel any action holding this card as a slot member (we'd otherwise
  // be deleting from under it).  delete_action_rows clears all the
  // action's CardHold rows including this one.
  if let Some(hold) = ctx.db.card_holds().card_id().find(&card_id) {
    delete_action_rows(ctx, hold.action_id);
  }
  // Defensive sweep in case an orphan CardHold remains (action was already gone).
  ctx.db.card_holds().card_id().delete(&card_id);
  ctx.db.cards().card_id().delete(&card_id);
  Ok(())
}
