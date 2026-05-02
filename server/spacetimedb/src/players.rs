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
  /// Layer the player's soul currently occupies (always a world layer).
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
