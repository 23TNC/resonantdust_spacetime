use crate::actions::{actions, Action};
use crate::cards::{cards, insert_card_row, Card};
use crate::packing::{pack_position, pack_zone, world_to_position, world_to_zone};
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
  definition_id: u16,

  #[serde(default)]
  flags: u64,

  #[serde(default)]
  q: Option<i32>,
  #[serde(default)]
  r: Option<i32>,
  #[serde(default)]
  z: Option<u16>,

  #[serde(default)]
  zone: Option<u32>,
  #[serde(default)]
  position: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct CardSeed {
  #[serde(default)]
  card_id: Option<u32>,

  card_type: u8,
  definition_id: u16,

  #[serde(default, alias = "soul_id_card_id")]
  soul_id: Option<u32>,

  #[serde(default, alias = "link_id_card_id", alias = "linked_id", alias = "linked_id_card_id", alias = "stack_id")]
  link_id: Option<u32>,

  #[serde(default)]
  player: Option<String>,

  #[serde(default)]
  flags: u64,

  #[serde(default)]
  q: Option<i32>,
  #[serde(default)]
  r: Option<i32>,
  #[serde(default)]
  z: Option<u16>,

  #[serde(default)]
  zone: Option<u32>,
  #[serde(default)]
  position: Option<u8>,
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

  #[serde(default, alias = "soul_id_card_id")]
  soul_id: Option<u32>,

  #[serde(default)]
  player: Option<String>,

  #[serde(default)]
  q: Option<i32>,
  #[serde(default)]
  r: Option<i32>,
  #[serde(default)]
  z: Option<u16>,

  #[serde(default)]
  zone: Option<u32>,
  #[serde(default)]
  position: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct ZoneSeed {
  #[serde(default)]
  zone: Option<u32>,

  #[serde(default)]
  zone_q: Option<i16>,
  #[serde(default)]
  zone_r: Option<i16>,
  #[serde(default)]
  z: Option<u16>,

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

fn pack_definition(card_type: u8, definition_id: u16) -> Result<u16, String> {
  if card_type > 0x0F {
    return Err(format!("card_type {} exceeds 4 bits", card_type));
  }
  if definition_id > 0x0FFF {
    return Err(format!("definition_id {} exceeds 12 bits", definition_id));
  }

  Ok(((card_type as u16) << 12) | definition_id)
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

fn resolve_zone_position(
  q: Option<i32>,
  r: Option<i32>,
  z: Option<u16>,
  zone: Option<u32>,
  position: Option<u8>,
  label: &str,
) -> Result<(u32, u8), String> {
  match (q, r, z, zone, position) {
    (Some(q), Some(r), Some(z), _, _) => {
      let (zone_q, zone_r) = world_to_zone(q, r);
      let (pos_q, pos_r) = world_to_position(q, r);
      Ok((pack_zone(zone_q, zone_r, z), pack_position(pos_q, pos_r)))
    }
    (_, _, _, Some(zone), Some(position)) => Ok((zone, position)),
    _ => Err(format!(
      "{label} must provide either q/r/z or zone/position"
    )),
  }
}

fn resolve_zone_seed(row: &ZoneSeed) -> Result<u32, String> {
  match (row.zone_q, row.zone_r, row.z, row.zone) {
    (Some(zone_q), Some(zone_r), Some(z), _) => Ok(pack_zone(zone_q, zone_r, z)),
    (_, _, _, Some(zone)) => Ok(zone),
    _ => Err("zone entry must provide either zone_q/zone_r/z or zone".to_string()),
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
    let zone = resolve_zone_seed(&row)?;
    let definition = pack_zone_definition(row.card_type, row.category)?;

    let zone_row = Zone {
      zone,
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

    if ctx.db.zones().zone().find(&zone).is_some() {
      ctx.db.zones().zone().update(zone_row);
    } else {
      ctx.db.zones().insert(zone_row);
    }
  }

  for row in data.player {
    let (zone, position) = resolve_zone_position(
      row.q,
      row.r,
      row.z,
      row.zone,
      row.position,
      &format!("player {}", row.name),
    )?;
    let definition = pack_definition(row.card_type, row.definition_id)?;

    if let Some(existing) = ctx.db.players().name().find(&row.name) {
      if let Some(mut card) = ctx.db.cards().card_id().find(&existing.soul_id) {
        card.definition = definition;
        card.soul_id = existing.soul_id;
        card.link_id = 0;
        card.flags = row.flags;
        card.zone = zone;
        card.position = position;
        ctx.db.cards().card_id().update(card);
      }

      ctx.db.players().player_id().update(Player {
        player_id: existing.player_id,
        name: existing.name,
        soul_id: existing.soul_id,
        zone,
        position,
      });
    } else {
      let inserted = ctx.db.cards().insert(Card {
        card_id: 0,
        definition,
        soul_id: 0,
        link_id: 0,
        flags: row.flags,
        zone,
        position,
      });

      let soul_id = inserted.card_id;
      let mut soul_card = inserted;
      soul_card.soul_id = soul_id;
      ctx.db.cards().card_id().update(soul_card);

      ctx.db.players().insert(Player {
        player_id: 0,
        name: row.name,
        soul_id,
        zone,
        position,
      });
    }
  }

  for row in data.card {
    let (zone, position) = match resolve_zone_position(
      row.q,
      row.r,
      row.z,
      row.zone,
      row.position,
      &format!("card {:?}", row.card_id),
    ) {
      Ok(zone_position) => zone_position,
      Err(_) => {
        let q = row.q.unwrap_or(0);
        let r = row.r.unwrap_or(0);
        let z = row.z.unwrap_or(0);
        let (zone_q, zone_r) = world_to_zone(q, r);
        let (pos_q, pos_r) = world_to_position(q, r);
        (pack_zone(zone_q, zone_r, z), pack_position(pos_q, pos_r))
      }
    };

    let definition = pack_definition(row.card_type, row.definition_id)?;

    let resolved_soul_id = match (row.soul_id, row.player.as_deref()) {
      (Some(_), Some(_)) => {
        return Err("card cannot specify both soul_id and player".to_string());
      }
      (Some(soul_id), None) => soul_id,
      (None, Some(player_name)) => {
        if let Some(player) = ctx.db.players().name().find(&player_name.to_string()) {
          player.soul_id
        } else {
          return Err(format!("player '{}' not found", player_name));
        }
      }
      (None, None) => 0,
    };

    let resolved_link_id = row.link_id.unwrap_or(0);

    match row.card_id {
      Some(card_id) => {
        if ctx.db.cards().card_id().find(&card_id).is_some() {
          ctx.db.cards().card_id().update(Card {
            card_id,
            definition,
            soul_id: resolved_soul_id,
            link_id: resolved_link_id,
            flags: row.flags,
            zone,
            position,
          });
        } else {
          ctx.db.cards().insert(Card {
            card_id,
            definition,
            soul_id: resolved_soul_id,
            link_id: resolved_link_id,
            flags: row.flags,
            zone,
            position,
          });
        }
      }
      None => {
        let q = row.q.unwrap_or(0);
        let r = row.r.unwrap_or(0);
        let z = row.z.unwrap_or(0);

        if row.zone.is_some() || row.position.is_some() {
          ctx.db.cards().insert(Card {
            card_id: 0,
            definition,
            soul_id: resolved_soul_id,
            link_id: resolved_link_id,
            flags: row.flags,
            zone,
            position,
          });
        } else {
          insert_card_row(
            ctx,
            row.card_type,
            row.definition_id,
            resolved_soul_id,
            resolved_link_id,
            row.flags,
            q,
            r,
            z,
          )?;
        }
      }
    }
  }

  for row in data.actions.into_iter().chain(data.action.into_iter()) {
    let (zone, position) = resolve_zone_position(
      row.q,
      row.r,
      row.z,
      row.zone,
      row.position,
      &format!("action {}", row.card_id),
    )?;

    let resolved_soul_id = match (row.soul_id, row.player.as_deref()) {
      (Some(_), Some(_)) => {
        return Err("action cannot specify both soul_id and player".to_string());
      }
      (Some(soul_id), None) => soul_id,
      (None, Some(player_name)) => {
        if let Some(player) = ctx.db.players().name().find(&player_name.to_string()) {
          player.soul_id
        } else {
          return Err(format!("player '{}' not found", player_name));
        }
      }
      (None, None) => 0,
    };

    if ctx.db.actions().card_id().find(&row.card_id).is_some() {
      ctx.db.actions().card_id().update(Action {
        card_id: row.card_id,
        recipe: row.recipe,
        start: row.start,
        end: row.end,
        flags: row.flags,
        soul_id: resolved_soul_id,
        zone,
        position,
      });
    } else {
      ctx.db.actions().insert(Action {
        card_id: row.card_id,
        recipe: row.recipe,
        start: row.start,
        end: row.end,
        flags: row.flags,
        soul_id: resolved_soul_id,
        zone,
        position,
      });
    }
  }

  Ok(())
}

#[reducer]
pub fn reset_and_bootstrap(ctx: &ReducerContext) -> Result<(), String> {
  for row in ctx.db.actions().iter() {
    ctx.db.actions().card_id().delete(&row.card_id);
  }

  for row in ctx.db.players().iter() {
    ctx.db.players().player_id().delete(&row.player_id);
  }

  for row in ctx.db.cards().iter() {
    ctx.db.cards().card_id().delete(&row.card_id);
  }

  for row in ctx.db.zones().iter() {
    ctx.db.zones().zone().delete(&row.zone);
  }

  bootstrap(ctx)
}
