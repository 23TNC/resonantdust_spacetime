use std::collections::{HashMap, HashSet};
use spacetimedb::{ReducerContext, ScheduleAt, Table, Timestamp};
use crate::cards::{cards, Card};
use crate::definitions::{Entity, ProductTarget, RecipeType, get_recipe, get_card_def,
  try_match_recipe_at, resolve_duration};
use crate::packing::{
  card_type_from_definition, definition_id_from_definition,
  is_stacked,
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

// ─── CardHold — internal slot-claim table, NOT sent to clients ───────────────
//
// Replaces the old `CARD_FLAG_SLOT_HOLD` bit on `Card.flags`.  When an action
// starts, every card in its claim window (root + slot range) gets a row here
// keyed by card_id.  The matcher checks this table when deciding whether a
// card is free to participate in a new recipe.  Cleanup is by-action_id —
// when an action ends, all its holds are deleted in one btree filter.  This
// makes the leak class from the old design (SLOT_HOLD bit set on a card
// whose action has been cancelled with a stale chain) structurally
// impossible: holds always go away with their action.
#[spacetimedb::table(accessor = card_holds)]
#[derive(Debug, Clone)]
pub struct CardHold {
  /// A card can be held by at most one action at a time (mirroring the
  /// old single-bit semantics).
  #[primary_key]
  pub card_id: u32,
  #[index(btree)]
  pub action_id: u32,
}

// ─── Adjacency packing helpers ───────────────────────────────────────────────

#[inline]
pub fn pack_participants(up_length: u8) -> u8 {
  (up_length & 0x0F) << 4
}

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

/// Delete an action, its scheduler row, and any CardHold rows it holds.
/// Single chokepoint for action removal — every cancel / complete path
/// goes through here, which is what makes hold-leak structurally
/// impossible (the holds belong to this action_id; we own the cleanup).
pub fn delete_action_rows(ctx: &ReducerContext, action_id: u32) {
  release_holds_for_action(ctx, action_id);
  for sched in ctx.db.action_scheduler().action_id().filter(&action_id) {
    ctx.db.action_scheduler().id().delete(&sched.id);
  }
  ctx.db.actions().action_id().delete(&action_id);
}

/// Insert one CardHold row per claimed card_id, all keyed to `action_id`.
fn claim_slots(ctx: &ReducerContext, action_id: u32, card_ids: &[u32]) {
  for &card_id in card_ids {
    // The PK is card_id; if a stale row exists (shouldn't, but defensive)
    // delete it first so insert doesn't fail.
    ctx.db.card_holds().card_id().delete(&card_id);
    ctx.db.card_holds().insert(CardHold { card_id, action_id });
  }
}

/// Wipe every CardHold belonging to `action_id`.  O(slot_count) via the
/// btree index on action_id.
fn release_holds_for_action(ctx: &ReducerContext, action_id: u32) {
  let ids: Vec<u32> = ctx.db.card_holds().action_id().filter(&action_id)
    .map(|h| h.card_id)
    .collect();
  for id in ids {
    ctx.db.card_holds().card_id().delete(&id);
  }
}

/// Snapshot all currently-held card_ids — used to feed `try_match_recipe_at`.
fn build_held_set(ctx: &ReducerContext) -> HashSet<u32> {
  ctx.db.card_holds().iter().map(|h| h.card_id).collect()
}

/// Run the adjacency matcher for a recipe at an actor card.  On success
/// returns `(participants, claimed_card_ids)` — the caller is responsible
/// for inserting the Action row and then calling `claim_slots(action_id,
/// claimed)` to record the holds keyed to the new action.  Returns None
/// if the recipe doesn't match. */
fn match_recipe(
  ctx:    &ReducerContext,
  recipe: &crate::definitions::RecipeDef,
  actor:  &Card,
) -> Option<(u8, Vec<u32>)> {
  let zone_cards: Vec<Card> = ctx.db.cards()
    .macro_zone()
    .filter(&actor.macro_zone)
    .filter(|c| c.layer == actor.layer)
    .collect();
  let chain  = build_chain(&zone_cards, actor, &recipe.recipe_type);
  let tuples = card_match_tuples(&chain);

  if recipe.slots.len() > MAX_ADJACENT_LENGTH as usize { return None; }

  let actor_pos = chain.iter().position(|c| c.card_id == actor.card_id).unwrap_or(0);
  let held = build_held_set(ctx);
  try_match_recipe_at(recipe, &tuples, actor_pos, &held)?;

  // Compute claimed card_ids: root (if rooted) + slot window.
  let mut claimed: Vec<u32> = Vec::new();
  if recipe.root.is_some() && !chain.is_empty() {
    claimed.push(chain[0].card_id);
  }
  let slot_end = (actor_pos + recipe.slots.len()).min(chain.len());
  for i in actor_pos..slot_end {
    let id = chain[i].card_id;
    if !claimed.contains(&id) { claimed.push(id); }
  }

  Some((pack_participants(recipe.slots.len() as u8), claimed))
}

/// Insert the Action row + ActionScheduler row.  Returns the new action_id
/// so the caller can hand it to `claim_slots`.
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
) -> Result<u32, String> {
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
  Ok(inserted.action_id)
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
  let (participants, claimed) = match_recipe(ctx, recipe_def, &actor)
    .ok_or_else(|| format!("recipe {recipe} did not match at card {card_id}"))?;
  let duration = crate::definitions::recipe_duration(recipe, 0);
  let action_id = insert_scheduled_action(
    ctx, card_id, owner_id, recipe,
    layer, macro_zone, micro_zone, duration, participants,
  )?;
  claim_slots(ctx, action_id, &claimed);
  Ok(())
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
  let (participants, claimed) = match_recipe(ctx, recipe_def, &actor)
    .ok_or_else(|| format!("recipe {recipe} did not match at card {card_id}"))?;
  let duration = resolve_duration(recipe_def, pool);
  let action_id = insert_scheduled_action(
    ctx, card_id, owner_id, recipe,
    layer, macro_zone, micro_zone, duration, participants,
  )?;
  claim_slots(ctx, action_id, &claimed);
  Ok(())
}

// start_action_inner_pool stays public because cards.rs:start_on_create_action
// still needs to fire on-create matches with a synthetic aspect pool.

// ─── Stack helpers ────────────────────────────────────────────────────────────

/// Walk cards stacked UP from `root_id`, returning all chained descendants
/// in traversal order.
fn collect_chain(all: &[Card], root_id: u32) -> Vec<Card> {
  let mut chain   = Vec::new();
  let mut parents = std::collections::HashSet::new();
  parents.insert(root_id);
  loop {
    let before = chain.len();
    for c in all {
      if is_stacked(c.flags)
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

/// Validate every field of a proposed `update_position` write before it
/// touches the card row.  Returns false → the move is silently dropped
/// from the batch.  Each rejection logs an error so misbehaving callers
/// surface in the server log.
///
/// Rules enforced (in order):
///
/// 1. **Card exists.**
/// 2. **Card not POSITION_LOCKED.**
/// 3. **Reserved flag bits (4-5) are zero.**
/// 4. **micro_zone reserved bits (0-1) are zero.**
/// 5. **Stack state is LOOSE or UP.**  (DOWN and ATTACHED were removed in
///    v2; persisting either of them would render nothing.)
/// 6. If state == UP:
///     a. **Not self-linked.**  (Trivial 1-cycle.)
///     b. **Parent (`micro_location`) exists.**
///     c. **Parent is STACKABLE.**
///     d. **Parent is in the same (layer, macro_zone, micro_zone)** the
///        new card row claims to be in.  A stacked card mirrors its
///        parent's anchor; if they diverge the chain is incoherent.
///     e. **No multi-hop cycle.**  Walk inward from the proposed parent
///        up to MAX_INWARD_HOPS; if we land on `card_id`, refuse.  An
///        exhausted walk also rejects (likely corrupt data).
fn validate_position_update(
  ctx:            &ReducerContext,
  card_id:        u32,
  layer:          u8,
  macro_zone:     u32,
  micro_zone:     u8,
  micro_location: u32,
  flags:          u16,
) -> bool {
  use crate::packing::{
    stack_state, is_stacked,
    STACK_STATE_LOOSE, STACK_STATE_UP,
    CARD_FLAG_STACKABLE, CARD_FLAG_POSITION_LOCKED,
  };

  // 1. Card must exist.
  let Some(current) = ctx.db.cards().card_id().find(&card_id) else {
    log::warn!("update_position: card {card_id} not found");
    return false;
  };

  // 2. Locked cards can't move.
  if current.flags & CARD_FLAG_POSITION_LOCKED != 0 {
    log::error!("update_position: refusing move on POSITION_LOCKED card {card_id}");
    return false;
  }

  // 3. Reserved flag bits (4-5) must stay zero so they remain available.
  const FLAG_RESERVED: u16 = 0b11 << 4;
  if flags & FLAG_RESERVED != 0 {
    log::error!("update_position: card {card_id} flags 0x{flags:04x} sets reserved bits 4-5");
    return false;
  }

  // 4. micro_zone bits 0-1 are reserved (the schema uses bits 2-7).
  if micro_zone & 0b11 != 0 {
    log::error!("update_position: card {card_id} micro_zone 0x{micro_zone:02x} sets reserved bits 0-1");
    return false;
  }

  // 5. Only LOOSE / UP are valid stack states in v2.
  let state = stack_state(flags);
  if state != STACK_STATE_LOOSE && state != STACK_STATE_UP {
    log::error!("update_position: card {card_id} stack_state {state} unsupported (LOOSE or UP only)");
    return false;
  }

  // 6. UP-specific structural rules.
  if state == STACK_STATE_UP {
    // 6a. No self-link.
    if micro_location == card_id {
      log::error!("update_position: refusing self-link on card {card_id}");
      return false;
    }

    // 6b. Parent must exist.
    let Some(parent) = ctx.db.cards().card_id().find(&micro_location) else {
      log::error!("update_position: card {card_id} parent {micro_location} not found");
      return false;
    };

    // 6c. Parent must be stackable.
    if parent.flags & CARD_FLAG_STACKABLE == 0 {
      log::error!("update_position: card {card_id} cannot stack on {} — parent is not STACKABLE", parent.card_id);
      return false;
    }

    // 6d. Parent must share (layer, macro_zone, micro_zone) with the
    //     proposed new state.  A stacked card mirrors its parent's anchor;
    //     a mismatch means the chain spans two zones.
    if parent.layer != layer || parent.macro_zone != macro_zone || parent.micro_zone != micro_zone {
      log::error!(
        "update_position: card {card_id} (layer={layer}, macro_zone={macro_zone}, micro_zone={micro_zone}) \
         doesn't match parent {} (layer={}, macro_zone={}, micro_zone={})",
        parent.card_id, parent.layer, parent.macro_zone, parent.micro_zone,
      );
      return false;
    }

    // 6e. Multi-hop cycle check.
    let mut ancestor = micro_location;
    for _ in 0..MAX_INWARD_HOPS {
      if ancestor == card_id {
        log::error!("update_position: card {card_id} would close a parent-chain cycle");
        return false;
      }
      let Some(a) = ctx.db.cards().card_id().find(&ancestor) else { return true; };
      if !is_stacked(a.flags) { return true; }
      ancestor = a.micro_location;
    }

    log::error!(
      "update_position: parent chain from card {card_id} exceeds {MAX_INWARD_HOPS} hops; refusing as suspected cycle / corrupt data"
    );
    return false;
  }

  true
}

/// Build the (card_id, CardDef) tuple list for adjacency matching from a
/// card slice.  Cards whose packed_definition has no registered def are
/// silently skipped.  Hold-status is no longer stored on the card — see
/// `CardHold` table.
fn card_match_tuples(cards: &[Card]) -> Vec<(u32, &'static crate::definitions::CardDef)> {
  cards.iter().filter_map(|c| {
    let ct  = card_type_from_definition(c.packed_definition);
    let did = definition_id_from_definition(c.packed_definition);
    let def = get_card_def(ct, did)?;
    Some((c.card_id, def))
  }).collect()
}

/// Build the chain used by the matcher from a zone-card snapshot.
/// For top_stack:          chain = [root, actor, outward...].
/// For on_create/explicit: chain = [actor].
pub fn build_chain(zone_cards: &[Card], actor: &Card, recipe_type: &RecipeType) -> Vec<Card> {
  match recipe_type {
    RecipeType::OnCreate | RecipeType::Explicit => vec![actor.clone()],
    RecipeType::TopStack => {
      let root = walk_inward_to_root(zone_cards, actor);
      let outward = collect_chain(zone_cards, root.card_id);
      let mut chain = vec![root];
      chain.extend(outward);
      chain
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
  // For any "root_*" target we need the chain ROOT's OWNER (the soul that
  // owns the root card), NOT the root card's own id.  Panel macro_zone
  // == soul_id; passing the root card's id when it's not itself a soul
  // creates an orphan panel that no client subscribes to and the
  // products accumulate invisibly forever.
  let root_owner = ctx.db.cards().card_id().find(&root_card_id)
    .map(|c| c.owner_id)
    .unwrap_or(actor_owner);

  match target {
    ProductTarget::ActorPanel => ProductDestination::Panel { owner_id: actor_owner },
    ProductTarget::RootPanel  => ProductDestination::Panel { owner_id: root_owner },

    ProductTarget::ActorWorld => world_dest_for_soul(ctx, actor_owner)
      .unwrap_or_else(|| {
        log::warn!("generate_products: actor_world unresolvable for soul {actor_owner}; falling back to actor_panel");
        ProductDestination::Panel { owner_id: actor_owner }
      }),

    ProductTarget::RootOwnerWorld => {
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
        // to the root's OWNER's panel (a real soul), not the root's card_id.
        log::warn!("generate_products: root_world target on panel root {root_card_id}; falling back to root_panel");
        ProductDestination::Panel { owner_id: root_owner }
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

    // Walk inward to the nearest surviving ancestor.  Bounded purely as a
    // sanity ceiling — `update_position` rejects writes that would create
    // a parent-chain cycle, so a real cycle should be unreachable.
    let mut ancestor = card.micro_location;
    for _ in 0..MAX_INWARD_HOPS {
      if !consumed.contains(&ancestor) { break; }
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

  splice_chain(ctx, &zone_cards, &consumed, scheduler_id);

  // delete_action_rows tears down the scheduler row, the action row, and
  // every CardHold the action owned.  No per-slot bit clearing needed —
  // holds are by-id now, not by-chain-walk.
  delete_action_rows(ctx, action_id);

  Ok(())
}

/// Cancel an action: tear down the action row, scheduler row, and all
/// CardHolds it owned.  With CardHold tracking the claims by id, this is
/// just `delete_action_rows` — no chain walk, no risk of clearing the
/// wrong cards.
fn cancel_action_internal(ctx: &ReducerContext, action_id: u32) {
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
// cancel + re-acquire without leaving stale CardHold rows.
//
// Re-match policy: after the move, the matcher runs on
//   1. the new (layer, macro_zone) bucket — for recipes the move just enabled
//   2. the old bucket if different — for recipes that were blocked by the
//      moving card and may now fire on what's left behind
// At each affected zone, we walk the zone's roots and try every TopStack
// recipe at every actor position past `actor_index` in greedy fashion
// (highest-weight wins, advance past the matched window).

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
///    actions still have CardHold rows, so the matcher can't double-claim
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
    if !validate_position_update(ctx, card_id, layer, macro_zone, micro_zone, micro_location, flags) {
      continue;
    }

    // validate_position_update already confirmed the card exists.
    let mut card = ctx.db.cards().card_id().find(&card_id).expect("validated above");
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

  // 3. Fire newly-eligible recipes in each affected zone.  CardHolds on
  //    surviving claims prevent double-firing.
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
/// are explicitly NOT a cancel trigger — CardHolds keep other actions
/// from displacing into the slot window during the action's lifetime,
/// and the player drag path only drops cards when the player commits the
/// move, at which point the chain length check captures any real removal.
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

/// Fire newly-matched TopStack recipes in `(layer, macro_zone)`.
///
/// Algorithm: enumerate every (root, recipe, start_idx) triple in the zone
/// that matches against the current state, pick the global best by weight
/// (with deterministic tiebreak on recipe.index then start_idx), fire it,
/// and repeat.  Each fire inserts CardHold rows that narrow the next
/// round's `held` set, so the loop terminates monotonically when no match
/// remains.
///
/// This replaces the previous greedy left-to-right walk per branch, which
/// could miss a higher-weight match further down the chain because it had
/// already committed to a lower-weight match at an earlier position.
///
/// Per-recipe eligibility:
/// - Rooted recipe (`recipe.root.is_some()`): fires only at `start_idx >= 1`
///   — slot window sits past the root precondition at chain[0].
/// - Root-less recipe: fires at any `start_idx >= 0`.
fn fire_matcher_in_zone(
  ctx:        &ReducerContext,
  layer:      u8,
  macro_zone: u32,
) -> Result<(), String> {
  // Hard ceiling on fires per zone-pass; a zone with this many concurrent
  // matches is pathological and we'd rather log + bail than spin.
  const MAX_FIRES_PER_PASS: u32 = 256;

  let zone_cards: Vec<Card> = ctx.db.cards()
    .macro_zone()
    .filter(&macro_zone)
    .filter(|c| c.layer == layer)
    .collect();

  let roots: Vec<Card> = zone_cards.iter()
    .filter(|c| !is_stacked(c.flags))
    .cloned()
    .collect();
  if roots.is_empty() { return Ok(()); }

  // Pre-build per-root chain + tuples once.  Card state in the zone
  // doesn't change between fires (start_action_inner only writes Action +
  // CardHold rows), so the chains stay valid; only the `held` set narrows
  // each iteration.
  struct ChainInfo {
    root:   Card,
    chain:  Vec<Card>,
    tuples: Vec<(u32, &'static crate::definitions::CardDef)>,
  }
  let chains: Vec<ChainInfo> = roots.into_iter()
    .map(|root| {
      let chain  = build_chain(&zone_cards, &root, &RecipeType::TopStack);
      let tuples = card_match_tuples(&chain);
      ChainInfo { root, chain, tuples }
    })
    .filter(|c| !c.chain.is_empty())
    .collect();

  for _ in 0..MAX_FIRES_PER_PASS {
    let held = build_held_set(ctx);

    // Enumerate every match across every chain and every start position.
    // `best` holds (weight, recipe.index, start_idx, chain_idx) — we tie-
    // break on (recipe.index, start_idx, chain_idx) so the choice is
    // deterministic regardless of HashMap iteration order.
    let mut best: Option<(u32, u16, usize, usize, &'static crate::definitions::RecipeDef)> = None;

    for (chain_idx, ci) in chains.iter().enumerate() {
      for recipe in crate::definitions::top_stack_recipes() {
        let min_start = if recipe.root.is_some() { 1 } else { 0 };
        for start_idx in min_start..ci.chain.len() {
          if held.contains(&ci.chain[start_idx].card_id) { continue; }
          let Some(result) = try_match_recipe_at(recipe, &ci.tuples, start_idx, &held) else { continue; };
          let key = (result.weight, recipe.index, start_idx, chain_idx);
          let better = match best {
            None => true,
            Some((bw, bi, bs, bc, _)) => {
              // Higher weight wins; on ties, lower recipe.index, then
              // lower start_idx, then lower chain_idx.  Strict ordering
              // so the choice is reproducible.
              if key.0 != bw            { key.0 > bw            }
              else if key.1 != bi       { key.1 < bi            }
              else if key.2 != bs       { key.2 < bs            }
              else                      { key.3 < bc            }
            }
          };
          if better { best = Some((key.0, key.1, key.2, key.3, recipe)); }
        }
      }
    }

    let Some((_, _, start_idx, chain_idx, recipe)) = best else { return Ok(()); };
    let actor = &chains[chain_idx].chain[start_idx];
    start_action_inner(
      ctx,
      actor.card_id, actor.owner_id, recipe.index,
      actor.layer, actor.macro_zone, actor.micro_zone,
    )?;
  }

  log::warn!(
    "fire_matcher_in_zone (layer={layer}, macro_zone={macro_zone}): hit MAX_FIRES_PER_PASS, bailing"
  );
  Ok(())
}

