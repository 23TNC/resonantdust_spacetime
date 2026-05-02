use crate::actions::actions;
use crate::cards::{cards, insert_card_row, insert_panel_card_row, Card};
use crate::definitions::find_def_by_str_id;
use crate::packing::{
  pack_definition, pack_macro_world, pack_micro_parent, with_stack_state,
  STACK_STATE_UP, WORLD_LAYER_GROUND,
  CARD_FLAG_STACKABLE, CARD_FLAG_POSITION_HOLD,
};
use crate::players::{players, upsert_player};
use crate::zones::{zones, Zone};
use serde::Deserialize;
use spacetimedb::{reducer, ReducerContext, Table};

const BOOTSTRAP_JSON: &str = include_str!("../data/bootstrap/bootstrap.json");

// ─── JSON shapes ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BootstrapData {
  #[serde(default)]
  player: Vec<PlayerSeed>,

  #[serde(default)]
  card: Vec<CardSeed>,

  #[serde(default)]
  zones: Vec<ZoneSeed>,
}

/// One player + the soul card that anchors them.  Looked up by string id
/// the same way `debug_spawn` does — no numeric card_type / definition_id.
#[derive(Debug, Deserialize)]
struct PlayerSeed {
  name: String,
  /// Soul card definition id (e.g. "human").  Resolved via
  /// `find_def_by_str_id`.
  id: String,
  #[serde(default)]
  q: Option<i32>,
  #[serde(default)]
  r: Option<i32>,
  #[serde(default, alias = "z")]
  layer: Option<u8>,
}

/// One non-soul card to seed.  Spawned exactly the way `debug_spawn` spawns
/// (string id lookup, category 0, CARD_FLAG_STACKABLE, panel placement by
/// default) unless world coords or `stacked_on` is provided.
#[derive(Debug, Deserialize)]
struct CardSeed {
  /// Card definition id (e.g. "corpus", "log").
  id: String,

  /// Owner soul.  Specify EITHER `owner_id` (raw card_id of the soul) OR
  /// `player` (player name to resolve to soul_id).
  #[serde(default, alias = "soul_id")]
  owner_id: Option<u32>,
  #[serde(default)]
  player:   Option<String>,

  /// Stack the new card UP onto this parent card_id.  Inherits parent's
  /// (layer, macro_zone, micro_zone); sets STACK_STATE_UP.  Mutually
  /// exclusive with `q`/`r`/`layer`.
  #[serde(default, alias = "stacked_on_up")]
  stacked_on: Option<u32>,

  /// World placement (mutually exclusive with `stacked_on`).  If any of
  /// these is set the card lands as a loose root at the world hex.  If
  /// none are set the card lands in the owner's panel.
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
  macro_zone: Option<u32>,

  #[serde(default)]
  zone_q: Option<i16>,
  #[serde(default)]
  zone_r: Option<i16>,
  #[serde(default, alias = "z")]
  layer: Option<u8>,

  card_type: u8,
  category:  u8,

  t0: u64,
  t1: u64,
  t2: u64,
  t3: u64,
  t4: u64,
  t5: u64,
  t6: u64,
  t7: u64,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn pack_zone_definition(card_type: u8, category: u8) -> Result<u8, String> {
  if card_type > 0x0F { return Err(format!("zone card_type {} exceeds 4 bits", card_type)); }
  if category  > 0x0F { return Err(format!("zone category {} exceeds 4 bits", category)); }
  Ok((card_type << 4) | category)
}

fn resolve_zone_seed(row: &ZoneSeed) -> Result<u32, String> {
  match (row.zone_q, row.zone_r, row.macro_zone) {
    (Some(zone_q), Some(zone_r), _) => Ok(pack_macro_world(zone_q, zone_r)),
    (_, _, Some(macro_zone))        => Ok(macro_zone),
    _ => Err("zone entry must provide zone_q/zone_r or macro_zone".to_string()),
  }
}

fn parse_bootstrap() -> Result<BootstrapData, String> {
  serde_json::from_str(BOOTSTRAP_JSON)
    .map_err(|e| format!("failed to parse bootstrap.json: {e}"))
}

fn resolve_owner(ctx: &ReducerContext, row: &CardSeed) -> Result<u32, String> {
  match (row.owner_id, row.player.as_deref()) {
    (Some(_), Some(_)) => Err("card seed cannot specify both owner_id and player".to_string()),
    (Some(owner_id), None) => Ok(owner_id),
    (None, Some(player_name)) => {
      ctx.db.players().name().find(&player_name.to_string())
        .map(|p| p.soul_id)
        .ok_or_else(|| format!("card seed: player '{}' not found", player_name))
    }
    (None, None) => Ok(0),
  }
}

/// Insert a card stacked UP on `parent_id`.  Inherits parent's
/// (layer, macro_zone, micro_zone); sets micro_location to packed
/// parent_id; sets STACK_STATE_UP in flags.
fn insert_stacked_seed(
  ctx:           &ReducerContext,
  card_type:     u8,
  definition_id: u8,
  owner_id:      u32,
  parent_id:     u32,
) -> Result<(), String> {
  let parent = ctx.db.cards().card_id().find(&parent_id)
    .ok_or_else(|| format!("card seed: parent card {} not found", parent_id))?;

  ctx.db.cards().insert(Card {
    card_id: 0,
    layer:          parent.layer,
    macro_zone:     parent.macro_zone,
    micro_zone:     parent.micro_zone,
    micro_location: pack_micro_parent(parent_id),
    owner_id,
    flags: with_stack_state(CARD_FLAG_STACKABLE, STACK_STATE_UP),
    packed_definition: pack_definition(card_type, 0, definition_id),
    data: 0,
  });

  Ok(())
}

fn spawn_player_seed(ctx: &ReducerContext, row: &PlayerSeed) -> Result<(), String> {
  let (card_type, definition_id) = find_def_by_str_id(&row.id)
    .ok_or_else(|| format!("player seed '{}': unknown card id '{}'", row.name, row.id))?;

  // Souls are pinned: a player can't drag their own soul card around the
  // world by accident.
  upsert_player(
    ctx, row.name.clone(),
    card_type, 0, definition_id, CARD_FLAG_POSITION_HOLD,
    row.q.unwrap_or(0), row.r.unwrap_or(0),
    row.layer.unwrap_or(WORLD_LAYER_GROUND),
  )
}

fn spawn_card_seed(ctx: &ReducerContext, row: &CardSeed) -> Result<(), String> {
  let (card_type, definition_id) = find_def_by_str_id(&row.id)
    .ok_or_else(|| format!("card seed: unknown card id '{}'", row.id))?;
  let owner_id = resolve_owner(ctx, row)?;

  // Stacked cards take precedence: position is derived from parent.
  if let Some(parent_id) = row.stacked_on {
    if row.q.is_some() || row.r.is_some() || row.layer.is_some() {
      return Err(format!(
        "card seed '{}': cannot specify both stacked_on and q/r/layer",
        row.id,
      ));
    }
    return insert_stacked_seed(ctx, card_type, definition_id, owner_id, parent_id);
  }

  // World placement: any of q/r/layer set.
  if row.q.is_some() || row.r.is_some() || row.layer.is_some() {
    insert_card_row(
      ctx, card_type, 0, definition_id, owner_id, CARD_FLAG_STACKABLE,
      row.q.unwrap_or(0), row.r.unwrap_or(0),
      row.layer.unwrap_or(WORLD_LAYER_GROUND),
    )?;
    return Ok(());
  }

  // Default: panel placement — same path as `debug_spawn`.
  insert_panel_card_row(ctx, card_type, 0, definition_id, owner_id, CARD_FLAG_STACKABLE)?;
  Ok(())
}

fn upsert_zone_seed(ctx: &ReducerContext, row: &ZoneSeed) -> Result<(), String> {
  let macro_zone = resolve_zone_seed(row)?;
  let layer      = row.layer.unwrap_or(WORLD_LAYER_GROUND);
  let definition = pack_zone_definition(row.card_type, row.category)?;

  let zone_row = Zone {
    layer, macro_zone, definition,
    t0: row.t0, t1: row.t1, t2: row.t2, t3: row.t3,
    t4: row.t4, t5: row.t5, t6: row.t6, t7: row.t7,
  };

  if ctx.db.zones().macro_zone().find(&macro_zone).is_some() {
    ctx.db.zones().macro_zone().update(zone_row);
  } else {
    ctx.db.zones().insert(zone_row);
  }
  Ok(())
}

// ─── Reducers ────────────────────────────────────────────────────────────────

#[reducer]
pub fn bootstrap(ctx: &ReducerContext) -> Result<(), String> {
  let data = parse_bootstrap()?;

  for row in &data.zones  { upsert_zone_seed(ctx, row)?; }
  for row in &data.player { spawn_player_seed(ctx, row)?; }
  for row in &data.card   { spawn_card_seed(ctx, row)?; }

  Ok(())
}

#[reducer]
pub fn reset_and_bootstrap(ctx: &ReducerContext) -> Result<(), String> {
  for row in ctx.db.actions().iter() { ctx.db.actions().action_id().delete(&row.action_id); }
  for row in ctx.db.players().iter() { ctx.db.players().player_id().delete(&row.player_id); }
  for row in ctx.db.cards().iter()   { ctx.db.cards().card_id().delete(&row.card_id); }
  for row in ctx.db.zones().iter()   { ctx.db.zones().macro_zone().delete(&row.macro_zone); }

  bootstrap(ctx)
}
