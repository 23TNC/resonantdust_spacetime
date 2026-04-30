use std::collections::HashMap;
use spacetimedb::{ReducerContext, ScheduleAt, Table, Timestamp};
use crate::cards::{cards, Card};
use crate::definitions::{Entity, ProductTarget, get_recipe, resolve_duration};
use crate::packing::{
  pack_macro_world, pack_micro_hex,
  card_type_from_definition, definition_id_from_definition,
  world_to_zone, world_to_position,
  CARD_FLAG_STACKED_UP, CARD_FLAG_STACKED_DOWN, CARD_FLAG_STACKABLE,
};

// ── ActionScheduler — internal scheduled table, not sent to clients ───────────

#[spacetimedb::table(accessor = action_scheduler, scheduled(complete_action))]
#[derive(Debug, Clone)]
pub struct ActionScheduler {
  #[primary_key]
  #[auto_inc]
  pub id: u64,
  pub scheduled_at: ScheduleAt,
  #[index(btree)]
  pub action_id: u32,
}

// ── Action — public table subscribed to by clients ────────────────────────────

#[spacetimedb::table(accessor = actions, public)]
#[derive(Debug, Clone)]
pub struct Action {
  #[primary_key]
  #[auto_inc]
  pub action_id: u32,
  #[index(btree)]
  pub card_id: u32,
  pub recipe: u16,
  pub end: u32,
  #[index(btree)]
  pub owner_id: u32,
  #[index(btree)]
  pub macro_location: u64,
  pub micro_location: u32,
}

pub fn current_seconds(ctx: &ReducerContext) -> Result<u32, String> {
  let micros = ctx.timestamp.to_micros_since_unix_epoch();
  if micros < 0 {
    return Err("ReducerContext timestamp is before Unix epoch".to_string());
  }
  let secs = (micros / 1_000_000) as u64;
  u32::try_from(secs).map_err(|_| "ReducerContext timestamp exceeds u32 seconds range".to_string())
}

/// Delete an action and its associated scheduler row.
pub fn delete_action_rows(ctx: &ReducerContext, action_id: u32) {
  for sched in ctx.db.action_scheduler().action_id().filter(&action_id) {
    ctx.db.action_scheduler().id().delete(&sched.id);
  }
  ctx.db.actions().action_id().delete(&action_id);
}

pub fn queue_action_inner(
  ctx:            &ReducerContext,
  card_id:        u32,
  owner_id:       u32,
  recipe:         u16,
  macro_location: u64,
  micro_location: u32,
) -> Result<(), String> {
  let inserted = ctx.db.actions().insert(Action {
    action_id: 0,
    card_id,
    recipe,
    end: 0,
    owner_id,
    macro_location,
    micro_location,
  });
  ctx.db.action_scheduler().insert(ActionScheduler {
    id:           0,
    scheduled_at: ScheduleAt::Time(Timestamp::from_micros_since_unix_epoch(i64::MAX)),
    action_id:    inserted.action_id,
  });
  Ok(())
}

fn insert_scheduled_action(
  ctx:            &ReducerContext,
  card_id:        u32,
  owner_id:       u32,
  recipe:         u16,
  macro_location: u64,
  micro_location: u32,
  duration:       u32,
) -> Result<(), String> {
  let now = current_seconds(ctx)?;
  let complete_at = Timestamp::from_micros_since_unix_epoch(
    ctx.timestamp.to_micros_since_unix_epoch()
      .saturating_add(duration as i64 * 1_000_000),
  );
  let inserted = ctx.db.actions().insert(Action {
    action_id: 0,
    card_id,
    recipe,
    end: now.saturating_add(duration),
    owner_id,
    macro_location,
    micro_location,
  });
  ctx.db.action_scheduler().insert(ActionScheduler {
    id:           0,
    scheduled_at: ScheduleAt::Time(complete_at),
    action_id:    inserted.action_id,
  });
  Ok(())
}

pub fn start_action_inner(
  ctx:            &ReducerContext,
  card_id:        u32,
  owner_id:       u32,
  recipe:         u16,
  macro_location: u64,
  micro_location: u32,
) -> Result<(), String> {
  let duration = crate::definitions::recipe_duration(recipe, 0);
  insert_scheduled_action(ctx, card_id, owner_id, recipe, macro_location, micro_location, duration)
}

/// Like `start_action_inner` but evaluates conditional durations against the
/// provided aspect pool rather than falling back to the first catch-all entry.
pub fn start_action_inner_pool(
  ctx:            &ReducerContext,
  card_id:        u32,
  owner_id:       u32,
  recipe:         u16,
  macro_location: u64,
  micro_location: u32,
  pool:           &HashMap<String, u32>,
) -> Result<(), String> {
  let recipe_def = get_recipe(recipe)
    .ok_or_else(|| format!("recipe {recipe} not found"))?;
  let duration = resolve_duration(recipe_def, pool);
  insert_scheduled_action(ctx, card_id, owner_id, recipe, macro_location, micro_location, duration)
}

#[spacetimedb::reducer]
pub fn queue_action(
  ctx: &ReducerContext,
  card_id: u32,
  owner_id: u32,
  recipe: u16,
  q: i32,
  r: i32,
  layer: u8,
) -> Result<(), String> {
  let (zone_q, zone_r) = world_to_zone(q, r);
  let (local_q, local_r) = world_to_position(q, r);
  queue_action_inner(
    ctx, card_id, owner_id, recipe,
    pack_macro_world(zone_q, zone_r, layer),
    pack_micro_hex(local_q, local_r),
  )
}

#[spacetimedb::reducer]
pub fn start_action(
  ctx:       &ReducerContext,
  action_id: u32,
) -> Result<(), String> {
  let mut action = ctx.db.actions().action_id().find(&action_id)
    .ok_or_else(|| format!("action {action_id} not found"))?;
  let now      = current_seconds(ctx)?;
  let duration = crate::definitions::recipe_duration(action.recipe, 0);
  let complete_at = Timestamp::from_micros_since_unix_epoch(
    ctx.timestamp.to_micros_since_unix_epoch()
      .saturating_add(duration as i64 * 1_000_000),
  );
  action.end = now.saturating_add(duration);
  ctx.db.actions().action_id().update(action);
  if let Some(mut sched) = ctx.db.action_scheduler().action_id().filter(&action_id).next() {
    sched.scheduled_at = ScheduleAt::Time(complete_at);
    ctx.db.action_scheduler().id().update(sched);
  }
  Ok(())
}

// ── Stack helpers ─────────────────────────────────────────────────────────────

fn collect_chain(all: &[Card], root_id: u32, direction: u16) -> Vec<Card> {
  let mut chain   = Vec::new();
  let mut parents = std::collections::HashSet::new();
  parents.insert(root_id);
  loop {
    let before = chain.len();
    for c in all {
      if c.flags & direction != 0
        && !chain.iter().any(|x: &Card| x.card_id == c.card_id)
        && parents.contains(&c.micro_location)
      {
        parents.insert(c.card_id);
        chain.push(c.clone());
      }
    }
    if chain.len() == before { break; }
  }
  chain
}

fn build_card_pool(cards: &[&Card]) -> HashMap<String, Vec<u32>> {
  let mut pool: HashMap<String, Vec<u32>> = HashMap::new();
  for card in cards {
    let ct  = card_type_from_definition(card.packed_definition);
    let did = definition_id_from_definition(card.packed_definition);
    if let Some(def) = crate::definitions::get_card_def(ct, did) {
      pool.entry(def.id.clone()).or_default().push(card.card_id);
      for aspect_name in def.aspects.keys() {
        pool.entry(aspect_name.clone()).or_default().push(card.card_id);
      }
    }
  }
  pool
}

fn consume_entity(entity: &Entity, pool: &mut HashMap<String, Vec<u32>>) -> Vec<u32> {
  match entity {
    Entity::Empty => vec![],

    Entity::Leaf { def_id, qty } => {
      let mut consumed = Vec::new();
      let mut need = *qty as usize;
      if def_id == "any" {
        let keys: Vec<String> = pool.keys().cloned().collect();
        'outer: for key in keys {
          if let Some(ids) = pool.get_mut(&key) {
            while need > 0 && !ids.is_empty() {
              consumed.push(ids.pop().unwrap());
              need -= 1;
            }
            if ids.is_empty() { pool.remove(&key); }
            if need == 0 { break 'outer; }
          }
        }
      } else if let Some(ids) = pool.get_mut(def_id.as_str()) {
        while need > 0 && !ids.is_empty() {
          consumed.push(ids.pop().unwrap());
          need -= 1;
        }
        if ids.is_empty() { pool.remove(def_id.as_str()); }
      }
      consumed
    }

    Entity::And { a, b } => {
      let mut v = consume_entity(a, pool);
      v.extend(consume_entity(b, pool));
      v
    }

    Entity::Or { a, b, .. } => {
      let snap = pool.clone();
      let va = consume_entity(a, pool);
      if va.is_empty() && !matches!(a.as_ref(), Entity::Empty) {
        *pool = snap;
        consume_entity(b, pool)
      } else {
        va
      }
    }
  }
}

fn generate_entity_products(
  ctx:       &ReducerContext,
  entity:    &Entity,
  target_id: u32,
  rng:       &mut u32,
) -> Result<(), String> {
  match entity {
    Entity::Empty => {}

    Entity::Leaf { def_id, qty } => {
      if def_id == "any" { return Ok(()); }
      match crate::definitions::find_def_by_str_id(def_id) {
        None => log::warn!("complete_action: unknown product '{def_id}'"),
        Some((card_type, definition_id)) => {
          for _ in 0..*qty {
            crate::cards::insert_panel_card_row(ctx, card_type, 0, definition_id, target_id, CARD_FLAG_STACKABLE)?;
          }
        }
      }
    }

    Entity::And { a, b } => {
      generate_entity_products(ctx, a, target_id, rng)?;
      generate_entity_products(ctx, b, target_id, rng)?;
    }

    Entity::Or { a, weights, b } => {
      *rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
      let [wa, wb] = weights;
      let total    = wa + wb;
      let pick_a   = total == 0 || (*rng % total) < *wa;
      if pick_a {
        generate_entity_products(ctx, a, target_id, rng)?;
      } else {
        generate_entity_products(ctx, b, target_id, rng)?;
      }
    }
  }
  Ok(())
}

fn generate_products(
  ctx:      &ReducerContext,
  recipe:   &crate::definitions::RecipeDef,
  owner_id: u32,
  card_id:  u32,
  rng:      &mut u32,
) -> Result<(), String> {
  for group in &recipe.products {
    let target_id = match group.target {
      ProductTarget::Owner => owner_id,
      ProductTarget::Root  => card_id,
    };
    generate_entity_products(ctx, &group.entity, target_id, rng)?;
  }
  Ok(())
}

// ── complete_action ───────────────────────────────────────────────────────────

#[spacetimedb::reducer]
pub fn complete_action(
  ctx:       &ReducerContext,
  scheduler: ActionScheduler,
) -> Result<(), String> {
  let scheduler_id = scheduler.id;
  let action_id    = scheduler.action_id;

  let action = ctx.db.actions().action_id().find(&action_id)
    .ok_or_else(|| format!("action {action_id} not found"))?;

  let recipe = get_recipe(action.recipe)
    .ok_or_else(|| format!("recipe {} not found", action.recipe))?;

  let root_card = ctx.db.cards().card_id().find(&action.card_id)
    .ok_or_else(|| format!("action card {} not found", action.card_id))?;
  let zone_cards: Vec<Card> = ctx.db.cards()
    .macro_location()
    .filter(&root_card.macro_location)
    .collect();

  let up_chain   = collect_chain(&zone_cards, root_card.card_id, CARD_FLAG_STACKED_UP);
  let down_chain = collect_chain(&zone_cards, root_card.card_id, CARD_FLAG_STACKED_DOWN);

  if !recipe.products.is_empty() {
    let mut rng = scheduler_id as u32;
    generate_products(ctx, recipe, action.owner_id, action.card_id, &mut rng)?;
  }

  if let Some(reagents) = &recipe.reagents {
    let all_stack: Vec<&Card> = std::iter::once(&root_card)
      .chain(up_chain.iter())
      .chain(down_chain.iter())
      .collect();
    let mut pool  = build_card_pool(&all_stack);
    let mut seen  = std::collections::HashSet::new();
    let to_delete = consume_entity(reagents, &mut pool);
    for cid in to_delete {
      if !seen.insert(cid) { continue; }
      for act in ctx.db.actions().card_id().filter(&cid) {
        if act.action_id != action_id {
          delete_action_rows(ctx, act.action_id);
        }
      }
      log::info!("complete_action {scheduler_id}: deleting card {cid}");
      ctx.db.cards().card_id().delete(&cid);
    }
  }

  ctx.db.action_scheduler().id().delete(&scheduler_id);
  ctx.db.actions().action_id().delete(&action_id);

  Ok(())
}

#[spacetimedb::reducer]
pub fn delete_action(
  ctx: &ReducerContext,
  action_id: u32,
) -> Result<(), String> {
  delete_action_rows(ctx, action_id);
  Ok(())
}
