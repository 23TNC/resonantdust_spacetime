use crate::actions::actions;
use crate::cards::{cards, Card};
use crate::packing::{
  pack_definition, pack_macro_world, pack_micro_zone, pack_micro_pixel, pack_micro_parent,
  with_stack_state, world_to_zone, world_to_position,
  STACK_STATE_LOOSE, STACK_STATE_UP, STACK_STATE_DOWN,
};
use crate::players::{players, Player};
use crate::zones::{zones, Zone};
use serde::Deserialize;
use spacetimedb::{reducer, ReducerContext, Table};

const BOOTSTRAP_JSON: &str = include_str!("../data/bootstrap/bootstrap.json");

#[derive(Debug, Deserialize)]
struct BootstrapData {
  #[serde(default)]
  player: Vec<PlayerSeed>,

  #[serde(default)]
  card: Vec<CardSeed>,

  #[serde(default)]
  zones: Vec<ZoneSeed>,
}

#[derive(Debug, Deserialize)]
struct PlayerSeed {
  name: String,
  card_type: u8,
  #[serde(default)]
  category: u8,
  definition_id: u8,

  #[serde(default)]
  flags: u16,

  #[serde(default)]
  q: Option<i32>,
  #[serde(default)]
  r: Option<i32>,
  #[serde(default, alias = "z")]
  layer: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct CardSeed {
  card_type: u8,
  #[serde(default)]
  category: u8,
  definition_id: u8,

  #[serde(default, alias = "soul_id")]
  owner_id: Option<u32>,

  #[serde(default)]
  player: Option<String>,

  #[serde(default)]
  flags: u16,

  #[serde(default)]
  q: Option<i32>,
  #[serde(default)]
  r: Option<i32>,
  #[serde(default, alias = "z")]
  layer: Option<u8>,

  #[serde(default)]
  pixel_x: Option<i16>,
  #[serde(default)]
  pixel_y: Option<i16>,

  /// Stack the new card UP onto this parent card_id.  Inherits parent's
  /// (layer, macro_zone, micro_zone); sets micro_location to packed parent_id;
  /// sets stack_state to UP in flags.  Mutually exclusive with `stacked_on_down`.
  #[serde(default, alias = "stacked_on_up")]
  stacked_on: Option<u32>,

  /// Stack the new card DOWN onto this parent card_id.  Same semantics as
  /// `stacked_on` but flips the direction.
  #[serde(default)]
  stacked_on_down: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ZoneSeed {
  #[serde(default)]
  macro_zone: Option<u32>,

  #[serde(default)]
  zone_q: Option<i16>,
  #[serde(default)]
  zone_r: Option<i16>,
  #[serde(default, alias = "z")]
  layer: Option<u8>,

  card_type: u8,
  category: u8,

  t0: u64,
  t1: u64,
  t2: u64,
  t3: u64,
  t4: u64,
  t5: u64,
  t6: u64,
  t7: u64,
}

fn pack_zone_definition(card_type: u8, category: u8) -> Result<u8, String> {
  if card_type > 0x0F {
    return Err(format!("zone card_type {} exceeds 4 bits", card_type));
  }
  if category > 0x0F {
    return Err(format!("zone category {} exceeds 4 bits", category));
  }
  Ok((card_type << 4) | category)
}

fn resolve_macro_micro(q: Option<i32>, r: Option<i32>) -> (u32, u8) {
  let q = q.unwrap_or(0);
  let r = r.unwrap_or(0);
  let (zone_q, zone_r) = world_to_zone(q, r);
  let (local_q, local_r) = world_to_position(q, r);
  (
    pack_macro_world(zone_q, zone_r),
    pack_micro_zone(local_q, local_r),
  )
}

fn resolve_zone_seed(row: &ZoneSeed) -> Result<u32, String> {
  match (row.zone_q, row.zone_r, row.macro_zone) {
    (Some(zone_q), Some(zone_r), _) => Ok(pack_macro_world(zone_q, zone_r)),
    (_, _, Some(macro_zone)) => Ok(macro_zone),
    _ => Err("zone entry must provide zone_q/zone_r or macro_zone".to_string()),
  }
}

fn parse_bootstrap() -> Result<BootstrapData, String> {
  serde_json::from_str(BOOTSTRAP_JSON)
    .map_err(|e| format!("failed to parse bootstrap.json: {e}"))
}

#[reducer]
pub fn bootstrap(ctx: &ReducerContext) -> Result<(), String> {
  let data = parse_bootstrap()?;

  for row in data.zones {
    let macro_zone = resolve_zone_seed(&row)?;
    let layer      = row.layer.unwrap_or(crate::packing::WORLD_LAYER_GROUND);
    let definition = pack_zone_definition(row.card_type, row.category)?;

    let zone_row = Zone {
      layer,
      macro_zone,
      definition,
      t0: row.t0,
      t1: row.t1,
      t2: row.t2,
      t3: row.t3,
      t4: row.t4,
      t5: row.t5,
      t6: row.t6,
      t7: row.t7,
    };

    if ctx.db.zones().macro_zone().find(&macro_zone).is_some() {
      ctx.db.zones().macro_zone().update(zone_row);
    } else {
      ctx.db.zones().insert(zone_row);
    }
  }

  for row in data.player {
    let (macro_zone, micro_zone) = resolve_macro_micro(row.q, row.r);
    let layer = row.layer.unwrap_or(crate::packing::WORLD_LAYER_GROUND);
    if let Some(existing) = ctx.db.players().name().find(&row.name) {
      if let Some(mut card) = ctx.db.cards().card_id().find(&existing.soul_id) {
        card.packed_definition = pack_definition(row.card_type, row.category, row.definition_id);
        card.owner_id   = existing.soul_id;
        card.flags      = with_stack_state(row.flags, STACK_STATE_LOOSE);
        card.layer      = layer;
        card.macro_zone = macro_zone;
        card.micro_zone = micro_zone;
        card.micro_location = pack_micro_pixel(0, 0);
        ctx.db.cards().card_id().update(card);
      }

      ctx.db.players().player_id().update(Player {
        player_id: existing.player_id,
        name:      existing.name,
        soul_id:   existing.soul_id,
        layer,
        macro_zone,
        micro_zone,
      });
    } else {
      let inserted = ctx.db.cards().insert(Card {
        card_id: 0,
        layer,
        macro_zone,
        micro_zone,
        micro_location: pack_micro_pixel(0, 0),
        owner_id: 0,
        flags: with_stack_state(row.flags, STACK_STATE_LOOSE),
        packed_definition: pack_definition(row.card_type, row.category, row.definition_id),
        data: 0,
        action_id: 0,
      });

      let soul_id = inserted.card_id;
      let mut soul_card = inserted;
      soul_card.owner_id = soul_id;
      ctx.db.cards().card_id().update(soul_card);

      ctx.db.players().insert(Player {
        player_id: 0,
        name: row.name,
        soul_id,
        layer,
        macro_zone,
        micro_zone,
      });
    }
  }

  for row in data.card {
    let resolved_owner_id = match (row.owner_id, row.player.as_deref()) {
      (Some(_), Some(_)) => {
        return Err("card cannot specify both owner_id and player".to_string());
      }
      (Some(owner_id), None) => owner_id,
      (None, Some(player_name)) => {
        if let Some(player) = ctx.db.players().name().find(&player_name.to_string()) {
          player.soul_id
        } else {
          return Err(format!("player '{}' not found", player_name));
        }
      }
      (None, None) => 0,
    };

    // Stacked cards take precedence: position is derived from parent.
    match (row.stacked_on, row.stacked_on_down) {
      (Some(_), Some(_)) => {
        return Err("card seed cannot specify both stacked_on and stacked_on_down".to_string());
      }
      (Some(parent_id), None) => {
        insert_stacked_seed(ctx, &row, resolved_owner_id, parent_id, STACK_STATE_UP)?;
        continue;
      }
      (None, Some(parent_id)) => {
        insert_stacked_seed(ctx, &row, resolved_owner_id, parent_id, STACK_STATE_DOWN)?;
        continue;
      }
      (None, None) => {}
    }

    match (row.pixel_x, row.pixel_y) {
      (Some(_), Some(_)) => {
        crate::cards::insert_panel_card_row(
          ctx,
          row.card_type, row.category, row.definition_id,
          resolved_owner_id, row.flags,
        )?;
      }
      _ => {
        crate::cards::insert_card_row(
          ctx,
          row.card_type, row.category, row.definition_id,
          resolved_owner_id, row.flags,
          row.q.unwrap_or(0), row.r.unwrap_or(0),
          row.layer.unwrap_or(crate::packing::WORLD_LAYER_GROUND),
        )?;
      }
    };
  }

  Ok(())
}

/// Insert a stacked seed card.  Inherits the parent's (layer, macro_zone,
/// micro_zone); sets micro_location to the packed parent_id; sets the
/// requested STACK_STATE in flags.
fn insert_stacked_seed(
  ctx:           &ReducerContext,
  row:           &CardSeed,
  owner_id:      u32,
  parent_id:     u32,
  state:         u8,
) -> Result<(), String> {
  let parent = ctx.db.cards().card_id().find(&parent_id)
    .ok_or_else(|| format!("card seed: parent card {} not found", parent_id))?;

  let flags = with_stack_state(row.flags, state);

  ctx.db.cards().insert(Card {
    card_id: 0,
    layer:          parent.layer,
    macro_zone:     parent.macro_zone,
    micro_zone:     parent.micro_zone,
    micro_location: pack_micro_parent(parent_id),
    owner_id,
    flags,
    packed_definition: pack_definition(row.card_type, row.category, row.definition_id),
    data: 0,
    action_id: 0,
  });

  Ok(())
}

#[reducer]
pub fn reset_and_bootstrap(ctx: &ReducerContext) -> Result<(), String> {
  for row in ctx.db.actions().iter() {
    ctx.db.actions().action_id().delete(&row.action_id);
  }

  for row in ctx.db.players().iter() {
    ctx.db.players().player_id().delete(&row.player_id);
  }

  for row in ctx.db.cards().iter() {
    ctx.db.cards().card_id().delete(&row.card_id);
  }

  for row in ctx.db.zones().iter() {
    ctx.db.zones().macro_zone().delete(&row.macro_zone);
  }

  bootstrap(ctx)
}
