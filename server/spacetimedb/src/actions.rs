use std::collections::{HashMap, HashSet};
use spacetimedb::{ReducerContext, ScheduleAt, Table, Timestamp};
use crate::cards::{cards, Card};
use crate::definitions::{Entity, ProductTarget, RecipeType, get_recipe, get_card_def,
  try_match_recipe_at, resolve_duration};
use crate::packing::{
  card_type_from_definition, definition_id_from_definition,
  stack_state, is_stacked,
  STACK_STATE_UP, STACK_STATE_DOWN,
  CARD_FLAG_SLOT_HOLD,
};

// ─── ActionScheduler — internal scheduled table, not sent to clients ─────────

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

// ─── Action — public table subscribed to by clients ──────────────────────────

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
  /// Subscription discriminator (mirrors actor card's layer).
  pub layer: u8,
  /// Mirrors actor card's macro_zone — panel soul_id or world (zone_q, zone_r).
  #[index(btree)]
  pub macro_zone: u32,
  /// Mirrors actor card's micro_zone.
  pub micro_zone: u8,
  /// Adjacency-encoded participant counts. bits[7:4] = up_length,
  /// bits[3:0] = down_length.  See `pack_participants`.
  pub participants: u8,
}

// ─── Adjacency packing helpers ───────────────────────────────────────────────

#[inline]
pub fn pack_participants(up_length: u8, down_length: u8) -> u8 {
  ((up_length & 0x0F) << 4) | (down_length & 0x0F)
}

#[inline]
#[allow(dead_code)]
pub fn participants_up(participants: u8) -> u8   { (participants >> 4) & 0x0F }

#[inline]
#[allow(dead_code)]
pub fn participants_down(participants: u8) -> u8 { participants & 0x0F }

/// Maximum adjacency length per branch (4 bits → 0..15).
pub const MAX_ADJACENT_LENGTH: u8 = 15;

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

/// Run the adjacency matcher for a recipe at an actor card.  On success,
/// sets `CARD_FLAG_SLOT_HOLD` on every chain card claimed by the recipe
/// (root + slot range) and returns the packed `participants: u8` to store
/// on the Action row.  Returns None if the recipe doesn't match.
fn match_and_hold(
  ctx:    &ReducerContext,
  recipe: &crate::definitions::RecipeDef,
  actor:  &Card,
) -> Option<u8> {
  // Pull all cards sharing the actor's macro_zone (and same layer).  Subscriptions
  // index on macro_zone alone — we filter by layer in-memory.
  let zone_cards: Vec<Card> = ctx.db.cards()
    .macro_zone()
    .filter(&actor.macro_zone)
    .filter(|c| c.layer == actor.layer)
    .collect();
  let chain  = build_chain(&zone_cards, actor, &recipe.recipe_type);
  let tuples = card_match_tuples(&chain);

  if recipe.slots.len() > MAX_ADJACENT_LENGTH as usize { return None; }

  // Resolve the actor's actual position in this chain.  For canonical fires
  // (greedy walk picked actor at chain[actor_index]) this equals
  // actor_index_for(recipe).  For non-canonical fires (e.g. a no-root
  // recipe firing at chain[2] in a CCCC merge) actor_pos > actor_index and
  // the matcher / SLOT_HOLD range need to track that.
  let actor_pos = chain.iter().position(|c| c.card_id == actor.card_id).unwrap_or(0);
  try_match_recipe_at(recipe, &tuples, actor_pos)?;

  if recipe.root.is_some() {
    set_slot_hold_range(ctx, &chain, 0..1);
  }
  set_slot_hold_range(ctx, &chain, actor_pos..actor_pos + recipe.slots.len());

  let length = recipe.slots.len() as u8;
  let participants = match recipe.recipe_type {
    RecipeType::TopStack    => pack_participants(length, 0),
    RecipeType::BottomStack => pack_participants(0, length),
    _                       => pack_participants(length, 0),
  };
  Some(participants)
}

fn insert_scheduled_action(
  ctx:          &ReducerContext,
  card_id:      u32,
  owner_id:     u32,
  recipe:       u16,
  layer:        u8,
  macro_zone:   u32,
  micro_zone:   u8,
  duration:     u32,
  participants: u8,
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
    layer,
    macro_zone,
    micro_zone,
    participants,
  });
  ctx.db.action_scheduler().insert(ActionScheduler {
    id:           0,
    scheduled_at: ScheduleAt::Time(complete_at),
    action_id:    inserted.action_id,
  });
  Ok(())
}

pub fn start_action_inner(
  ctx:        &ReducerContext,
  card_id:    u32,
  owner_id:   u32,
  recipe:     u16,
  layer:      u8,
  macro_zone: u32,
  micro_zone: u8,
) -> Result<(), String> {
  let recipe_def = get_recipe(recipe)
    .ok_or_else(|| format!("recipe {recipe} not found"))?;
  let actor = ctx.db.cards().card_id().find(&card_id)
    .ok_or_else(|| format!("actor card {card_id} not found"))?;
  let participants = match_and_hold(ctx, recipe_def, &actor)
    .ok_or_else(|| format!("recipe {recipe} did not match at card {card_id}"))?;
  let duration = crate::definitions::recipe_duration(recipe, 0);
  insert_scheduled_action(
    ctx, card_id, owner_id, recipe,
    layer, macro_zone, micro_zone, duration, participants,
  )
}

/// Like `start_action_inner` but evaluates conditional durations against the
/// provided aspect pool rather than falling back to the first catch-all entry.
pub fn start_action_inner_pool(
  ctx:        &ReducerContext,
  card_id:    u32,
  owner_id:   u32,
  recipe:     u16,
  layer:      u8,
  macro_zone: u32,
  micro_zone: u8,
  pool:       &HashMap<String, u32>,
) -> Result<(), String> {
  let recipe_def = get_recipe(recipe)
    .ok_or_else(|| format!("recipe {recipe} not found"))?;
  let actor = ctx.db.cards().card_id().find(&card_id)
    .ok_or_else(|| format!("actor card {card_id} not found"))?;
  let participants = match_and_hold(ctx, recipe_def, &actor)
    .ok_or_else(|| format!("recipe {recipe} did not match at card {card_id}"))?;
  let duration = resolve_duration(recipe_def, pool);
  insert_scheduled_action(
    ctx, card_id, owner_id, recipe,
    layer, macro_zone, micro_zone, duration, participants,
  )
}

// (start_action_now / queue_action / start_action removed in Phase 8 cleanup —
//  the Phase 5 sync protocol routes everything through update_position, which
//  invokes the matcher via match_and_hold + start_action_inner directly.
//  start_action_inner_pool stays public because cards.rs:start_on_create_action
//  still needs to fire on-create matches with a synthetic aspect pool.)

// ─── Stack helpers ────────────────────────────────────────────────────────────

/// Walk cards stacked in `direction` (STACK_STATE_UP or STACK_STATE_DOWN)
/// from `root_id`, returning all chained descendants in traversal order.
fn collect_chain(all: &[Card], root_id: u32, direction: u8) -> Vec<Card> {
  let mut chain   = Vec::new();
  let mut parents = std::collections::HashSet::new();
  parents.insert(root_id);
  loop {
    let before = chain.len();
    for c in all {
      if stack_state(c.flags) == direction
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

/// Walk inward from `card` (following parent_id links via micro_location)
/// to the root.  Returns the same card if it's already a root.  Bounded.
const MAX_INWARD_HOPS: u32 = 64;
fn walk_inward_to_root(zone_cards: &[Card], card: &Card) -> Card {
  let mut current = card.clone();
  let mut hops    = 0u32;
  while hops < MAX_INWARD_HOPS {
    if !is_stacked(current.flags) { return current; }
    let parent_id = current.micro_location;
    match zone_cards.iter().find(|c| c.card_id == parent_id) {
      Some(parent) => { current = parent.clone(); hops += 1; }
      None         => return current,
    }
  }
  current
}

/// Build the (card_id, CardDef, flags) tuple list for adjacency matching
/// from a card slice.  Cards whose packed_definition has no registered def
/// are silently skipped.
fn card_match_tuples(cards: &[Card]) -> Vec<(u32, &'static crate::definitions::CardDef, u16)> {
  cards.iter().filter_map(|c| {
    let ct  = card_type_from_definition(c.packed_definition);
    let did = definition_id_from_definition(c.packed_definition);
    let def = get_card_def(ct, did)?;
    Some((c.card_id, def, c.flags))
  }).collect()
}

/// Pick the outward direction for a recipe given the actor's flags.
/// If the actor is itself stacked, follow its existing direction.
/// Otherwise fall back to the recipe type's natural direction.
pub fn outward_direction_for(actor: &Card, recipe_type: &RecipeType) -> u8 {
  match stack_state(actor.flags) {
    STACK_STATE_DOWN => STACK_STATE_DOWN,
    STACK_STATE_UP   => STACK_STATE_UP,
    _ => match recipe_type {
      RecipeType::BottomStack => STACK_STATE_DOWN,
      _                       => STACK_STATE_UP,
    },
  }
}

/// Build the chain used by the matcher from a zone-card snapshot.
/// For top_stack/bottom_stack: chain = [root, actor, outward...].
/// For on_create/explicit:     chain = [actor].
pub fn build_chain(zone_cards: &[Card], actor: &Card, recipe_type: &RecipeType) -> Vec<Card> {
  match recipe_type {
    RecipeType::OnCreate | RecipeType::Explicit => vec![actor.clone()],
    RecipeType::TopStack | RecipeType::BottomStack => {
      let direction = outward_direction_for(actor, recipe_type);
      let root = walk_inward_to_root(zone_cards, actor);
      let outward = collect_chain(zone_cards, root.card_id, direction);
      let mut chain = vec![root];
      chain.extend(outward);
      chain
    }
  }
}

/// Set `CARD_FLAG_SLOT_HOLD` on every card in `chain[range]` (clamped).
fn set_slot_hold_range(ctx: &ReducerContext, chain: &[Card], range: std::ops::Range<usize>) {
  for i in range {
    if i >= chain.len() { break; }
    let mut c = chain[i].clone();
    if c.flags & CARD_FLAG_SLOT_HOLD == 0 {
      c.flags |= CARD_FLAG_SLOT_HOLD;
      ctx.db.cards().card_id().update(c);
    }
  }
}

/// Clear `CARD_FLAG_SLOT_HOLD` on every card in `chain[range]` whose row
/// still exists.  Idempotent.
fn clear_slot_hold_range(ctx: &ReducerContext, chain: &[Card], range: std::ops::Range<usize>) {
  for i in range {
    if i >= chain.len() { break; }
    let card_id = chain[i].card_id;
    if let Some(mut c) = ctx.db.cards().card_id().find(&card_id) {
      if c.flags & CARD_FLAG_SLOT_HOLD != 0 {
        c.flags &= !CARD_FLAG_SLOT_HOLD;
        ctx.db.cards().card_id().update(c);
      }
    }
  }
}

/// Where a single product card row should be inserted.
///
/// `Panel(soul_id)` → standard inventory placement via `insert_panel_card_row`.
/// `World { layer, macro_zone, micro_zone, owner_id }` → loose root in the
/// world at the given hex.  `owner_id` is whichever soul "produced" the card
/// (used for trade/audit; subscriptions don't filter on it for world cards).
enum ProductDestination {
  Panel  { owner_id: u32 },
  World  { layer: u8, macro_zone: u32, micro_zone: u8, owner_id: u32 },
}

fn generate_entity_products(
  ctx:    &ReducerContext,
  entity: &Entity,
  dest:   &ProductDestination,
  rng:    &mut u32,
) -> Result<(), String> {
  match entity {
    Entity::Empty => {}

    Entity::Leaf { def_id, qty } => {
      if def_id == "any" { return Ok(()); }
      match crate::definitions::find_def_by_str_id(def_id) {
        None => log::warn!("complete_action: unknown product '{def_id}'"),
        Some((card_type, definition_id)) => {
          for _ in 0..*qty {
            insert_product(ctx, card_type, definition_id, dest)?;
          }
        }
      }
    }

    Entity::And { a, b } => {
      generate_entity_products(ctx, a, dest, rng)?;
      generate_entity_products(ctx, b, dest, rng)?;
    }

    Entity::Or { a, weights, b } => {
      *rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
      let [wa, wb] = weights;
      let total    = wa + wb;
      let pick_a   = total == 0 || (*rng % total) < *wa;
      if pick_a {
        generate_entity_products(ctx, a, dest, rng)?;
      } else {
        generate_entity_products(ctx, b, dest, rng)?;
      }
    }
  }
  Ok(())
}

/// Insert a single product card row at the resolved destination.  Routes
/// to `insert_panel_card_row` for panel placements; for world placements
/// uses `insert_world_card_row_at` so the row lands at a specific
/// `(layer, macro_zone, micro_zone)` rather than a (q, r, layer) triple.
fn insert_product(
  ctx:           &ReducerContext,
  card_type:     u8,
  definition_id: u8,
  dest:          &ProductDestination,
) -> Result<(), String> {
  let stackable = crate::packing::CARD_FLAG_STACKABLE;
  match dest {
    ProductDestination::Panel { owner_id } => {
      crate::cards::insert_panel_card_row(ctx, card_type, 0, definition_id, *owner_id, stackable)?;
    }
    ProductDestination::World { layer, macro_zone, micro_zone, owner_id } => {
      crate::cards::insert_world_card_row_at(
        ctx, card_type, 0, definition_id, *owner_id, stackable,
        *layer, *macro_zone, *micro_zone,
      )?;
    }
  }
  Ok(())
}

/// Produce cards into the destinations declared by `recipe.products`.
///
/// Resolution per target:
/// - `ActorPanel`     → `actor_owner`'s inventory (the soul running the recipe).
/// - `RootPanel`      → `root_card_id` panel — for an actor that's also a soul,
///                       this is the same as ActorPanel; for action cards held
///                       by other souls, it puts the product in the chain root's
///                       owner panel.
/// - `ActorWorld`     → world hex of the action owner's soul card.
/// - `RootOwnerWorld` → world hex of the chain root's owner soul card.
/// - `RootWorld`      → the chain root card's own world position (only valid
///                       when the root is itself in the world; falls back to
///                       ActorWorld with a warning otherwise).
///
/// All world placements drop the card as a LOOSE root with a (0, 0) cosmetic
/// pixel offset.  Sub-hex placement rules (find an empty cell, scatter,
/// etc.) are deferred — the simplest "land on the same hex" semantics is
/// enough for current recipes; future work can refine.
fn generate_products(
  ctx:          &ReducerContext,
  recipe:       &crate::definitions::RecipeDef,
  actor_owner:  u32,
  root_card_id: u32,
  rng:          &mut u32,
) -> Result<(), String> {
  for group in &recipe.products {
    let dest = resolve_product_destination(ctx, &group.target, actor_owner, root_card_id);
    generate_entity_products(ctx, &group.entity, &dest, rng)?;
  }
  Ok(())
}

fn resolve_product_destination(
  ctx:          &ReducerContext,
  target:       &ProductTarget,
  actor_owner:  u32,
  root_card_id: u32,
) -> ProductDestination {
  match target {
    ProductTarget::ActorPanel => ProductDestination::Panel { owner_id: actor_owner },
    ProductTarget::RootPanel  => ProductDestination::Panel { owner_id: root_card_id },

    ProductTarget::ActorWorld => world_dest_for_soul(ctx, actor_owner)
      .unwrap_or_else(|| {
        log::warn!("generate_products: actor_world unresolvable for soul {actor_owner}; falling back to actor_panel");
        ProductDestination::Panel { owner_id: actor_owner }
      }),

    ProductTarget::RootOwnerWorld => {
      let root = ctx.db.cards().card_id().find(&root_card_id);
      let root_owner = root.as_ref().map(|c| c.owner_id).unwrap_or(actor_owner);
      world_dest_for_soul(ctx, root_owner).unwrap_or_else(|| {
        log::warn!("generate_products: root_owner_world unresolvable for soul {root_owner}; falling back to actor_panel");
        ProductDestination::Panel { owner_id: actor_owner }
      })
    }

    ProductTarget::RootWorld => {
      let Some(root) = ctx.db.cards().card_id().find(&root_card_id) else {
        log::warn!("generate_products: root_world has no root card {root_card_id}; falling back to actor_panel");
        return ProductDestination::Panel { owner_id: actor_owner };
      };
      if crate::packing::is_world_layer(root.layer) {
        ProductDestination::World {
          layer:      root.layer,
          macro_zone: root.macro_zone,
          micro_zone: root.micro_zone,
          owner_id:   root.owner_id,
        }
      } else {
        // Root is in a panel — there's no world hex to drop into.  Fall back
        // to that panel so products at least materialise somewhere visible.
        log::warn!("generate_products: root_world target on panel root {root_card_id}; falling back to root_panel");
        ProductDestination::Panel { owner_id: root_card_id }
      }
    }
  }
}

/// Look up `soul_id`'s soul card and return a World destination at its
/// current world hex.  Returns None if the soul card isn't found or isn't
/// on a world layer.
fn world_dest_for_soul(ctx: &ReducerContext, soul_id: u32) -> Option<ProductDestination> {
  let soul = ctx.db.cards().card_id().find(&soul_id)?;
  if !crate::packing::is_world_layer(soul.layer) { return None; }
  Some(ProductDestination::World {
    layer:      soul.layer,
    macro_zone: soul.macro_zone,
    micro_zone: soul.micro_zone,
    owner_id:   soul_id,
  })
}

/// After reagents are consumed, re-link any surviving stacked card whose
/// parent was deleted to its nearest surviving ancestor.
fn splice_chain(
  ctx:          &ReducerContext,
  zone_cards:   &[Card],
  consumed:     &HashSet<u32>,
  scheduler_id: u64,
) {
  if consumed.is_empty() { return; }

  let surviving_ids: HashSet<u32> = zone_cards.iter()
    .filter(|c| !consumed.contains(&c.card_id))
    .map(|c| c.card_id)
    .collect();

  let parent_map: HashMap<u32, u32> = zone_cards.iter()
    .filter(|c| is_stacked(c.flags))
    .map(|c| (c.card_id, c.micro_location))
    .collect();

  for card in zone_cards {
    if consumed.contains(&card.card_id) { continue; }
    if !is_stacked(card.flags) { continue; }
    if !consumed.contains(&card.micro_location) { continue; }

    let mut ancestor = card.micro_location;
    while consumed.contains(&ancestor) {
      match parent_map.get(&ancestor).copied() {
        Some(p) => ancestor = p,
        None    => break,
      }
    }

    if !consumed.contains(&ancestor) && surviving_ids.contains(&ancestor) {
      let mut spliced = card.clone();
      spliced.micro_location = ancestor;
      log::info!(
        "complete_action {scheduler_id}: spliced card {} parent {} -> {}",
        card.card_id, card.micro_location, ancestor,
      );
      ctx.db.cards().card_id().update(spliced);
    }
  }
}

// ─── complete_action ──────────────────────────────────────────────────────────

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

  let action_card = ctx.db.cards().card_id().find(&action.card_id)
    .ok_or_else(|| format!("action card {} not found", action.card_id))?;
  let zone_cards: Vec<Card> = ctx.db.cards()
    .macro_zone()
    .filter(&action_card.macro_zone)
    .filter(|c| c.layer == action_card.layer)
    .collect();

  let chain = build_chain(&zone_cards, &action_card, &recipe.recipe_type);
  let actor_index = crate::definitions::actor_index_for(recipe);
  let actor_pos   = chain.iter()
    .position(|c| c.card_id == action_card.card_id)
    .unwrap_or(0);

  if !recipe.products.is_empty() {
    let mut rng = scheduler_id as u32;
    let root_id = chain.first().map(|c| c.card_id).unwrap_or(action.card_id);
    generate_products(ctx, recipe, action.owner_id, root_id, &mut rng)?;
  }

  // Reagent indexing is actor-position-relative under Phase 10.  The
  // recipe author writes reagent N meaning "the card at slot position
  // (N - actor_index)" — for canonical fires (actor_pos == actor_index)
  // this collapses to chain[N], the historical absolute behavior.  When
  // the recipe fires at a non-canonical actor position (e.g. CC merging
  // onto another CC, where the second action's actor sits at chain[2]),
  // the slot window shifts with it and reagent N targets
  // chain[actor_pos + (N - actor_index)].
  //
  // Reagents below `actor_index` (only meaningful for rooted recipes
  // declaring `reagents: [0]` to consume the root) target chain[N]
  // absolute, since the root precondition is always at chain[0].
  let mut consumed: HashSet<u32> = HashSet::new();
  for &reagent_idx in &recipe.reagents {
    let r = reagent_idx as usize;
    let target_idx = if r < actor_index { r } else { actor_pos + (r - actor_index) };
    if target_idx >= chain.len() {
      log::warn!(
        "complete_action {scheduler_id}: reagent {} → chain idx {} out of range (chain.len = {}, actor_pos = {})",
        reagent_idx, target_idx, chain.len(), actor_pos,
      );
      continue;
    }
    let card_id = chain[target_idx].card_id;
    for act in ctx.db.actions().card_id().filter(&card_id) {
      if act.action_id != action_id {
        cancel_action_internal(ctx, act.action_id);
      }
    }
    log::info!("complete_action {scheduler_id}: consuming reagent card {card_id}");
    ctx.db.cards().card_id().delete(&card_id);
    consumed.insert(card_id);
  }

  // Slot release range is also actor-position-relative.  Root release
  // (when present) is always chain[0].
  let slot_start = actor_pos;
  let slot_end   = (actor_pos + recipe.slots.len()).min(chain.len());
  if recipe.root.is_some() && !chain.is_empty() {
    let card_id = chain[0].card_id;
    if !consumed.contains(&card_id) {
      if let Some(mut c) = ctx.db.cards().card_id().find(&card_id) {
        if c.flags & CARD_FLAG_SLOT_HOLD != 0 {
          c.flags &= !CARD_FLAG_SLOT_HOLD;
          ctx.db.cards().card_id().update(c);
        }
      }
    }
  }
  for i in slot_start..slot_end {
    let card_id = chain[i].card_id;
    if consumed.contains(&card_id) { continue; }
    if let Some(mut c) = ctx.db.cards().card_id().find(&card_id) {
      if c.flags & CARD_FLAG_SLOT_HOLD != 0 {
        c.flags &= !CARD_FLAG_SLOT_HOLD;
        ctx.db.cards().card_id().update(c);
      }
    }
  }

  splice_chain(ctx, &zone_cards, &consumed, scheduler_id);

  ctx.db.action_scheduler().id().delete(&scheduler_id);
  ctx.db.actions().action_id().delete(&action_id);

  Ok(())
}

/// Internal cancel — clears SLOT_HOLD on participants and deletes the
/// action row.  Same flow as the public `cancel_action` reducer.
fn cancel_action_internal(ctx: &ReducerContext, action_id: u32) {
  let action = match ctx.db.actions().action_id().find(&action_id) {
    Some(a) => a,
    None    => return,
  };
  let recipe = match get_recipe(action.recipe) {
    Some(r) => r,
    None    => { delete_action_rows(ctx, action_id); return; }
  };
  if let Some(actor) = ctx.db.cards().card_id().find(&action.card_id) {
    let zone_cards: Vec<Card> = ctx.db.cards()
      .macro_zone()
      .filter(&actor.macro_zone)
      .filter(|c| c.layer == actor.layer)
      .collect();
    let chain = build_chain(&zone_cards, &actor, &recipe.recipe_type);
    let actor_pos = chain.iter()
      .position(|c| c.card_id == actor.card_id)
      .unwrap_or(0);
    let slot_end = (actor_pos + recipe.slots.len()).min(chain.len());
    if recipe.root.is_some() {
      clear_slot_hold_range(ctx, &chain, 0..1);
    }
    clear_slot_hold_range(ctx, &chain, actor_pos..slot_end);
  }
  delete_action_rows(ctx, action_id);
}

/// Public cancel-an-action reducer.  Server-internal cancel from
/// `update_position` / `complete_action` go through `cancel_action_internal`
/// directly; this entry point stays available for admin / debug paths
/// (e.g. cancelling a stuck action by id from the SpacetimeDB CLI).  The
/// in-game sync protocol does not call this — `update_position` cancels
/// disturbed actions implicitly when their claim window is touched.
#[spacetimedb::reducer]
pub fn delete_action(
  ctx: &ReducerContext,
  action_id: u32,
) -> Result<(), String> {
  cancel_action_internal(ctx, action_id);
  Ok(())
}

// ─── update_position — single client-facing position-change reducer ───────────
//
// Phase 5 sync protocol: the client sends position updates (and only position
// updates).  This reducer applies the change, cancels any actions whose
// participants the change disturbed, then re-runs the matcher on every
// affected zone to start newly-eligible recipes.
//
// Cancellation policy: any action whose claim window includes the moving
// card is cancelled immediately, BEFORE the position is applied.  Re-running
// the matcher afterwards may restart it if the new chain still satisfies it,
// so a no-op move (player drags then drops in place) round-trips through
// cancel + re-acquire without leaving stale SLOT_HOLD bits.
//
// Re-match policy: after the move, the matcher runs on
//   1. the new (layer, macro_zone) bucket — for recipes the move just enabled
//   2. the old bucket if different — for recipes that were blocked by the
//      moving card and may now fire on what's left behind
// At each affected zone, we walk the zone's roots and try every TopStack /
// BottomStack recipe at every actor position past `actor_index` in greedy
// fashion (highest-weight wins, advance past the matched window).

#[spacetimedb::reducer]
pub fn update_position(
  ctx:            &ReducerContext,
  card_id:        u32,
  layer:          u8,
  macro_zone:     u32,
  micro_zone:     u8,
  micro_location: u32,
  flags:          u16,
) -> Result<(), String> {
  apply_moves_and_match(
    ctx,
    &[(card_id, layer, macro_zone, micro_zone, micro_location, flags)],
  )
}

/// Batched variant — applies a tuple of position updates atomically, then
/// runs the matcher once per affected zone (deduplicated).  Used by stack-
/// merge drops where multiple cards' mirrored positions change in one
/// player gesture.
#[spacetimedb::reducer]
pub fn update_positions(
  ctx:             &ReducerContext,
  card_ids:        Vec<u32>,
  layers:          Vec<u8>,
  macro_zones:     Vec<u32>,
  micro_zones:     Vec<u8>,
  micro_locations: Vec<u32>,
  flags:           Vec<u16>,
) -> Result<(), String> {
  let n = card_ids.len();
  if layers.len() != n
    || macro_zones.len() != n
    || micro_zones.len() != n
    || micro_locations.len() != n
    || flags.len() != n
  {
    return Err(format!(
      "update_positions: length mismatch — card_ids={} layers={} macro_zones={} micro_zones={} micro_locations={} flags={}",
      n, layers.len(), macro_zones.len(), micro_zones.len(), micro_locations.len(), flags.len(),
    ));
  }
  let moves: Vec<(u32, u8, u32, u8, u32, u16)> = (0..n)
    .map(|i| (card_ids[i], layers[i], macro_zones[i], micro_zones[i], micro_locations[i], flags[i]))
    .collect();
  apply_moves_and_match(ctx, &moves)
}

/// Shared core for update_position / update_positions.  Each tuple is
/// `(card_id, layer, macro_zone, micro_zone, micro_location, flags)`.
///
/// Phase 10 model:
/// 1. Apply every position update first (so subsequent passes see the
///    post-move state).
/// 2. For each affected zone, walk all actions and cancel only those whose
///    chain is now structurally broken (actor gone, or chain shorter than
///    `actor_pos + recipe.slots.len()`).  Actions whose claimed cards still
///    sit adjacent to the actor — even at a different chain position
///    after a merge — survive untouched.  This is a strict relaxation of
///    the Phase 5.F cancel-on-disturb rule: a player drag that merely
///    relocates the slot window (e.g. CC merging onto another CC) keeps
///    both pre-existing actions running.
/// 3. Run the matcher in every affected zone.  Cards held by surviving
///    actions still have `SLOT_HOLD` set, so the matcher can't double-claim
///    them — newly-eligible recipes only fire on cards left over.
fn apply_moves_and_match(
  ctx:   &ReducerContext,
  moves: &[(u32, u8, u32, u8, u32, u16)],
) -> Result<(), String> {
  use std::collections::HashSet;

  let mut affected: HashSet<(u8, u32)> = HashSet::new();

  // 1a. Capture each card's current (layer, macro_zone) so we can check
  //     actions in the OLD zone after the move (the move may have shifted
  //     the card to a new macro_zone — both sides need a sweep).
  for &(card_id, layer, macro_zone, _, _, _) in moves {
    if let Some(card) = ctx.db.cards().card_id().find(&card_id) {
      affected.insert((card.layer, card.macro_zone));
    }
    affected.insert((layer, macro_zone));
  }

  // 1b. Apply position updates.
  for &(card_id, layer, macro_zone, micro_zone, micro_location, flags) in moves {
    let Some(mut card) = ctx.db.cards().card_id().find(&card_id) else {
      log::warn!("update_position: card {card_id} not found");
      continue;
    };
    card.layer          = layer;
    card.macro_zone     = macro_zone;
    card.micro_zone     = micro_zone;
    card.micro_location = micro_location;
    card.flags          = flags;
    ctx.db.cards().card_id().update(card.clone());
    crate::cards::sync_player_location_for_soul_card_pub(ctx, &card);
  }

  // 2. Cancel only actions whose chain is now broken.
  for (layer, macro_zone) in &affected {
    cancel_broken_actions_in_zone(ctx, *layer, *macro_zone);
  }

  // 3. Fire newly-eligible recipes in each affected zone.  SLOT_HOLD on
  //    surviving claims prevents double-firing.
  for (layer, macro_zone) in affected {
    fire_matcher_in_zone(ctx, layer, macro_zone)?;
  }

  Ok(())
}

/// Cancel any action in `(layer, macro_zone)` whose chain no longer has
/// room for the recipe's slot window starting at the actor's current
/// position.  An action is considered broken iff:
///   - its actor card was deleted, or
///   - chain length from the actor's chain root is less than
///     `actor_pos + recipe.slots.len()` — i.e. cards that were claimed
///     have left the chain.
///
/// Slot-identity changes (a different card now occupies a claimed slot)
/// are explicitly NOT a cancel trigger — Phase 10 trusts SLOT_HOLD to keep
/// other actions from displacing into the slot window during the action's
/// lifetime, and the player drag path only drops cards when the player
/// commits the move, at which point the chain length check captures any
/// real removal.
fn cancel_broken_actions_in_zone(ctx: &ReducerContext, layer: u8, macro_zone: u32) {
  let zone_cards: Vec<Card> = ctx.db.cards()
    .macro_zone()
    .filter(&macro_zone)
    .filter(|c| c.layer == layer)
    .collect();

  let actions: Vec<Action> = ctx.db.actions()
    .macro_zone()
    .filter(&macro_zone)
    .filter(|a| a.layer == layer)
    .collect();

  for action in actions {
    let Some(recipe) = get_recipe(action.recipe) else {
      cancel_action_internal(ctx, action.action_id);
      continue;
    };
    let Some(actor) = ctx.db.cards().card_id().find(&action.card_id) else {
      // Actor card is gone → chain definitionally broken.
      cancel_action_internal(ctx, action.action_id);
      continue;
    };

    let chain = build_chain(&zone_cards, &actor, &recipe.recipe_type);
    let actor_pos = chain.iter()
      .position(|c| c.card_id == actor.card_id)
      .unwrap_or(0);
    let needed_len = actor_pos + recipe.slots.len();

    if chain.len() < needed_len {
      cancel_action_internal(ctx, action.action_id);
      continue;
    }

    // Rooted recipes additionally require chain[0] (the root) to exist.
    if recipe.root.is_some() && chain.is_empty() {
      cancel_action_internal(ctx, action.action_id);
    }
  }
}

/// Fire any newly-matched TopStack / BottomStack recipes anchored in
/// `(layer, macro_zone)`.  Walks each root in the zone, builds the chain
/// for each branch direction, and greedily activates the highest-weight
/// recipe at each actor position past the root.  Already-running recipes
/// (cards already SLOT_HOLD'd) are skipped via the matcher's own check.
fn fire_matcher_in_zone(
  ctx:        &ReducerContext,
  layer:      u8,
  macro_zone: u32,
) -> Result<(), String> {
  let zone_cards: Vec<Card> = ctx.db.cards()
    .macro_zone()
    .filter(&macro_zone)
    .filter(|c| c.layer == layer)
    .collect();

  // Identify roots: cards in this zone that are not stacked on a parent.
  let roots: Vec<Card> = zone_cards.iter()
    .filter(|c| !is_stacked(c.flags))
    .cloned()
    .collect();

  for root in &roots {
    fire_matcher_branch(ctx, &zone_cards, root, &RecipeType::TopStack)?;
    fire_matcher_branch(ctx, &zone_cards, root, &RecipeType::BottomStack)?;
  }

  Ok(())
}

/// Greedy actor-position walk for one branch direction off `root`.  At each
/// chain position, tries every eligible recipe of `recipe_type` and starts
/// the highest-weight match.  Advance past the matched slot window and
/// repeat until we run out of chain.
///
/// Per-recipe eligibility (Phase 10):
/// - **Rooted recipe** (`recipe.root.is_some()`): fires at any `start_idx >= 1`
///   so the slot window sits past the root precondition (which always lives
///   at chain[0]).
/// - **Root-less recipe** (`recipe.root.is_none()`): fires at any
///   `start_idx >= 0`.  The chain root IS the actor when start_idx == 0;
///   for start_idx > 0 the actor is whichever card the slot window starts
///   on.  This is what lets two CC pairs both fire after merging into CCCC.
fn fire_matcher_branch(
  ctx:         &ReducerContext,
  zone_cards:  &[Card],
  root:        &Card,
  recipe_type: &RecipeType,
) -> Result<(), String> {
  let chain = build_chain(zone_cards, root, recipe_type);
  if chain.is_empty() { return Ok(()); }

  let mut start_idx = 0usize;

  while start_idx < chain.len() {
    let actor = &chain[start_idx];
    if actor.flags & CARD_FLAG_SLOT_HOLD != 0 {
      start_idx += 1;
      continue;
    }

    // Find the best-matching recipe of this type at the current chain position.
    let tuples = card_match_tuples(&chain);
    let mut best: Option<(&'static crate::definitions::RecipeDef, u32)> = None;
    for recipe in recipes_of_type(recipe_type) {
      // Rooted recipes need the root (chain[0]) distinct from the slot
      // window, so they can't fire at start_idx == 0; root-less recipes
      // have no such constraint.
      if recipe.root.is_some() && start_idx == 0 { continue; }

      if let Some(result) = try_match_recipe_at(recipe, &tuples, start_idx) {
        if best.map_or(true, |(_, w)| result.weight > w) {
          best = Some((recipe, result.weight));
        }
      }
    }

    if let Some((recipe, _)) = best {
      // start_action_inner will re-run match_and_hold against the live zone,
      // which is the same chain we just matched against (we haven't mutated
      // anything yet).  Set SLOT_HOLD via that path so subsequent greedy
      // iterations skip the now-claimed window.
      start_action_inner(
        ctx,
        actor.card_id,
        actor.owner_id,
        recipe.index,
        actor.layer,
        actor.macro_zone,
        actor.micro_zone,
      )?;
      start_idx += recipe.slots.len().max(1);
    } else {
      start_idx += 1;
    }
  }

  Ok(())
}

fn recipes_of_type(t: &RecipeType) -> Box<dyn Iterator<Item = &'static crate::definitions::RecipeDef>> {
  match t {
    RecipeType::TopStack    => Box::new(crate::definitions::top_stack_recipes()),
    RecipeType::BottomStack => Box::new(crate::definitions::bottom_stack_recipes()),
    RecipeType::OnCreate    => Box::new(crate::definitions::on_create_recipes()),
    // Explicit recipes are player-triggered, not auto-fired by position changes.
    RecipeType::Explicit    => Box::new(std::iter::empty()),
  }
}

