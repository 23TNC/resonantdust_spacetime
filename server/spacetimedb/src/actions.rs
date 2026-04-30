use std::collections::HashMap;
use spacetimedb::{ReducerContext, Table};
use crate::cards::{cards, Card};
use crate::definitions::{Entity, get_recipe};
use crate::packing::{
  pack_macro_world, pack_micro_hex,
  card_type_from_definition, definition_id_from_definition,
  world_to_zone, world_to_position,
  CARD_FLAG_STACKED_UP, CARD_FLAG_STACKED_DOWN, CARD_FLAG_STACKABLE,
};

pub const ACTION_FLAG_STARTED:   u8 = 1 << 0;
pub const ACTION_FLAG_COMPLETED: u8 = 1 << 1;

#[spacetimedb::table(accessor = actions, public)]
#[derive(Debug, Clone)]
pub struct Action {
  #[primary_key]
  #[auto_inc]
  pub action_id: u32,
  #[index(btree)]
  pub card_id: u32,
  pub recipe: u16,
  pub start: u32,
  pub end: u32,
  pub flags: u8,
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
  let now = current_seconds(ctx)?;

  ctx.db.actions().insert(Action {
    action_id: 0,
    card_id,
    recipe,
    start: now,
    end: 0,
    flags: 0,
    owner_id,
    macro_location: pack_macro_world(zone_q, zone_r, layer),
    micro_location: pack_micro_hex(local_q, local_r),
  });

  Ok(())
}

#[spacetimedb::reducer]
pub fn start_action(
  ctx:      &ReducerContext,
  card_id:  u32,
  recipe:   u16,
  owner_id: u32,
) -> Result<(), String> {
  let now      = current_seconds(ctx)?;
  let duration = crate::definitions::recipe_duration(recipe, 0);
  ctx.db.actions().insert(Action {
    action_id:      0,
    card_id,
    recipe,
    start:          now,
    end:            now.saturating_add(duration),
    flags:          ACTION_FLAG_STARTED,
    owner_id,
    macro_location: 0,
    micro_location: 0,
  });
  Ok(())
}

// ── Stack helpers ─────────────────────────────────────────────────────────────

/// Collect all cards chained from `root_id` in one direction (STACKED_UP or
/// STACKED_DOWN). For stacked cards `micro_location` is the parent card_id.
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

/// Build a pool mapping definition string id → [card_id, ...] from a card slice.
fn build_card_pool(cards: &[&Card]) -> HashMap<String, Vec<u32>> {
  let mut pool: HashMap<String, Vec<u32>> = HashMap::new();
  for card in cards {
    let ct  = card_type_from_definition(card.packed_definition);
    let did = definition_id_from_definition(card.packed_definition);
    if let Some(def) = crate::definitions::get_card_def(ct, did) {
      pool.entry(def.id.clone()).or_default().push(card.card_id);
    }
  }
  pool
}

/// Consume cards matching `entity` from `pool`. Returns the card_ids removed.
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
      // Try A first; fall back to B if A yields nothing (and A isn't Empty).
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

/// Recursively generate product cards into the owner's panel.
/// `rng` is advanced on each OR node so successive weighted picks differ.
fn generate_products(
  ctx:      &ReducerContext,
  entity:   &Entity,
  owner_id: u32,
  rng:      &mut u32,
) -> Result<(), String> {
  match entity {
    Entity::Empty => {}

    Entity::Leaf { def_id, qty } => {
      if def_id == "any" { return Ok(()); }
      match crate::definitions::find_def_by_str_id(def_id) {
        None => log::warn!("complete_action: unknown product '{def_id}'"),
        Some((card_type, definition_id)) => {
          for _ in 0..*qty {
            crate::cards::insert_panel_card_row(ctx, card_type, 0, definition_id, owner_id, CARD_FLAG_STACKABLE)?;
          }
        }
      }
    }

    Entity::And { a, b } => {
      generate_products(ctx, a, owner_id, rng)?;
      generate_products(ctx, b, owner_id, rng)?;
    }

    Entity::Or { a, weights, b } => {
      *rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
      let [wa, wb] = weights;
      let total    = wa + wb;
      let pick_a   = total == 0 || (*rng % total) < *wa;
      if pick_a {
        generate_products(ctx, a, owner_id, rng)?;
      } else {
        generate_products(ctx, b, owner_id, rng)?;
      }
    }
  }
  Ok(())
}

// ── complete_action ───────────────────────────────────────────────────────────

#[spacetimedb::reducer]
pub fn complete_action(
  ctx: &ReducerContext,
  action_id: u32,
) -> Result<(), String> {
  // 1. Validate state.
  let action = ctx.db.actions().action_id().find(&action_id)
    .ok_or_else(|| format!("action {action_id} not found"))?;
  if action.flags & ACTION_FLAG_STARTED == 0 {
    return Err(format!("action {action_id} is not in started state"));
  }

  // 2. Look up the recipe.
  let recipe = get_recipe(action.recipe)
    .ok_or_else(|| format!("recipe {} not found", action.recipe))?;

  // 3. Get the root card and every other card sharing its macro_location.
  let root_card = ctx.db.cards().card_id().find(&action.card_id)
    .ok_or_else(|| format!("action card {} not found", action.card_id))?;
  let zone_cards: Vec<Card> = ctx.db.cards()
    .macro_location()
    .filter(&root_card.macro_location)
    .collect();

  // 4. Collect the up-chain and down-chain independently.
  let up_chain   = collect_chain(&zone_cards, root_card.card_id, CARD_FLAG_STACKED_UP);
  let down_chain = collect_chain(&zone_cards, root_card.card_id, CARD_FLAG_STACKED_DOWN);

  // 5. Generate products into the owner's panel BEFORE consuming reagents,
  //    because the root card itself may be a reagent and could be deleted.
  if let Some(products) = &recipe.products {
    let mut rng = action_id;
    generate_products(ctx, products, action.owner_id, &mut rng)?;
  }

  // 6. Consume reagents — delete cards that satisfy the reagent entity.
  if let Some(reagents) = &recipe.reagents {
    let all_stack: Vec<&Card> = std::iter::once(&root_card)
      .chain(up_chain.iter())
      .chain(down_chain.iter())
      .collect();
    let mut pool     = build_card_pool(&all_stack);
    let to_delete    = consume_entity(reagents, &mut pool);
    for cid in to_delete {
      for act in ctx.db.actions().card_id().filter(&cid) {
        if act.action_id != action_id {
          ctx.db.actions().action_id().delete(&act.action_id);
        }
      }
      log::info!("complete_action {action_id}: deleting card {cid}");
      ctx.db.cards().card_id().delete(&cid);
    }
  }

  // 7. Delete the completed action.
  ctx.db.actions().action_id().delete(&action_id);

  Ok(())
}

#[spacetimedb::reducer]
pub fn delete_action(
  ctx: &ReducerContext,
  action_id: u32,
) -> Result<(), String> {
  ctx.db.actions().action_id().delete(&action_id);
  Ok(())
}
