use crate::actions::{actions, Action};
use crate::cards::{cards, Card};
use crate::packing::{
  pack_definition, pack_macro_world, pack_micro_hex,
  world_to_zone, world_to_position,
};
use crate::players::{players, Player};
use crate::zones::{zones, Zone};
use serde::Deserialize;
use spacetimedb::{reducer, ReducerContext, Table};

const BOOTSTRAP_JSON: &str = include_str!("../bootstrap/bootstrap.json");

#[derive(Debug, Deserialize)]
struct BootstrapData {
  #[serde(default)]
  player: Vec<PlayerSeed>,

  #[serde(default)]
  card: Vec<CardSeed>,

  #[serde(default)]
  action: Vec<ActionSeed>,

  #[serde(default)]
  actions: Vec<ActionSeed>,

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

  // Panel placement: owner's panel at pixel (x, y), layer z (default 1).
  // Mutually exclusive with q/r world placement.
  #[serde(default)]
  pixel_x: Option<i16>,
  #[serde(default)]
  pixel_y: Option<i16>,

  // Raw extra bits written into data / data2.
  #[serde(default)]
  extra: Option<u64>,
  #[serde(default)]
  extra2: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ActionSeed {
  card_id: u32,
  recipe: u16,

  #[serde(default)]
  start: u32,
  #[serde(default)]
  end: u32,
  #[serde(default)]
  flags: u8,

  #[serde(default, alias = "soul_id")]
  owner_id: Option<u32>,

  #[serde(default)]
  player: Option<String>,

  #[serde(default)]
  q: Option<i32>,
  #[serde(default)]
  r: Option<i32>,
  #[serde(default, alias = "z")]
  layer: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct ZoneSeed {
  #[serde(default)]
  macro_location: Option<u64>,

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

fn resolve_macro_micro(q: Option<i32>, r: Option<i32>, layer: Option<u8>) -> (u64, u32) {
  let q = q.unwrap_or(0);
  let r = r.unwrap_or(0);
  let layer = layer.unwrap_or(0);
  let (zone_q, zone_r) = world_to_zone(q, r);
  let (local_q, local_r) = world_to_position(q, r);
  (
    pack_macro_world(zone_q, zone_r, layer),
    pack_micro_hex(local_q, local_r),
  )
}

fn resolve_zone_seed(row: &ZoneSeed) -> Result<u64, String> {
  match (row.zone_q, row.zone_r, row.layer, row.macro_location) {
    (Some(zone_q), Some(zone_r), layer, _) => {
      Ok(pack_macro_world(zone_q, zone_r, layer.unwrap_or(0)))
    }
    (_, _, _, Some(macro_location)) => Ok(macro_location),
    _ => Err("zone entry must provide zone_q/zone_r or macro_location".to_string()),
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
    let macro_location = resolve_zone_seed(&row)?;
    let definition = pack_zone_definition(row.card_type, row.category)?;

    let zone_row = Zone {
      macro_location,
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

    if ctx.db.zones().macro_location().find(&macro_location).is_some() {
      ctx.db.zones().macro_location().update(zone_row);
    } else {
      ctx.db.zones().insert(zone_row);
    }
  }

  for row in data.player {
    let (macro_location, micro_location) =
      resolve_macro_micro(row.q, row.r, row.layer);
    if let Some(existing) = ctx.db.players().name().find(&row.name) {
      if let Some(mut card) = ctx.db.cards().card_id().find(&existing.soul_id) {
        card.packed_definition = pack_definition(row.card_type, row.category, row.definition_id);
        card.owner_id = existing.soul_id;
        card.flags = row.flags;
        card.macro_location = macro_location;
        card.micro_location = micro_location;
        ctx.db.cards().card_id().update(card);
      }

      ctx.db.players().player_id().update(Player {
        player_id: existing.player_id,
        name: existing.name,
        soul_id: existing.soul_id,
        macro_location,
        micro_location,
      });
    } else {
      let inserted = ctx.db.cards().insert(Card {
        card_id: 0,
        macro_location,
        micro_location,
        owner_id: 0,
        flags: row.flags,
        packed_definition: pack_definition(row.card_type, row.category, row.definition_id),
        data: 0,
        data2: 0,
      });

      let soul_id = inserted.card_id;
      let mut soul_card = inserted;
      soul_card.owner_id = soul_id;
      ctx.db.cards().card_id().update(soul_card);

      ctx.db.players().insert(Player {
        player_id: 0,
        name: row.name,
        soul_id,
        macro_location,
        micro_location,
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
          row.q.unwrap_or(0), row.r.unwrap_or(0), row.layer.unwrap_or(0),
        )?;
      }
    };
  }

  for row in data.actions.into_iter().chain(data.action.into_iter()) {
    let (macro_location, micro_location) =
      resolve_macro_micro(row.q, row.r, row.layer);

    let resolved_owner_id = match (row.owner_id, row.player.as_deref()) {
      (Some(_), Some(_)) => {
        return Err("action cannot specify both owner_id and player".to_string());
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

    ctx.db.actions().insert(Action {
      action_id: 0,
      card_id: row.card_id,
      recipe: row.recipe,
      start: row.start,
      end: row.end,
      flags: row.flags,
      owner_id: resolved_owner_id,
      macro_location,
      micro_location,
    });
  }

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
    ctx.db.zones().macro_location().delete(&row.macro_location);
  }

  bootstrap(ctx)
}
