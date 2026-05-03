//! Action machinery — recipes that match against card stacks, schedule
//! their completion, consume reagents, and produce outputs.
//!
//! # Trigger model
//!
//! The server does not walk the `cards` table to reconstruct stacks.
//! Inventory layout is client-side, and stack composition reaches the
//! server only through `submit_inventory_stacks` (top/bottom stacks) and
//! `insert_card_row` (on_create). This module exposes the *helpers*
//! those code paths call — but no client-callable reducers of its own.
//!
//! Helpers (called from `cards.rs`):
//!
//! - [`try_start_top_stack_action`] — caller passes the submitted root +
//!   up-branch; we try every `TopStack` recipe and start one if a match
//!   fits.
//! - [`try_start_bottom_stack_action`] — same shape for the down-branch.
//! - [`try_start_on_create_action`] — caller passes a freshly-inserted
//!   card; we try every `OnCreate` recipe whose `root` matches that card.
//! - [`cancel_actions_for_cards`] — caller passes the card_ids in a
//!   submitted stack; any action with a [`CardHold`] for one of those
//!   cards is cancelled.
//!
//! # Why no public reducers
//!
//! Earlier iterations exposed `start_action` / `delete_action` as public
//! reducers. That was a security hole: any connected client could call
//! them with arbitrary arguments and either spawn product cards or
//! cancel another player's actions. By keeping the action lifecycle
//! purely *implicit* — driven by validated stack submissions and card
//! creations — the only way a client influences action state is via the
//! reducers it's already authenticated against (`submit_inventory_stacks`,
//! `insert_card_row` paths). A malicious client can submit nonsense
//! stacks; the server rejects them at the membership / validation layer
//! before any action helper runs.
//!
//! # Tables
//!
//! - [`Action`] (public): one row per in-progress action. Clients
//!   subscribe by `macro_zone == own_player_id` to see their own.
//! - [`ActionScheduler`] (private, scheduled): drives the `complete_action`
//!   reducer when an action's duration elapses. The reducer is annotated
//!   `#[reducer]` because SpacetimeDB requires that for scheduled-table
//!   callbacks; defensive checks at the top guard against client-spoofed
//!   early invocation.
//! - [`CardHold`] (private): tracks which cards are claimed by which
//!   action. Prevents double-booking and provides the cancellation index.
//!
//! # Completion
//!
//! When the scheduler fires, `complete_action`:
//!
//! 1. Verifies the call is legitimate (scheduler row exists in the
//!    table, action's end-time has passed) — rejects spoofed early
//!    completions.
//! 2. Generates products into the configured targets (`root_panel`,
//!    `actor_panel`).
//! 3. Deletes the cards listed in `recipe.reagents`.
//! 4. Tears down the action, scheduler row, and all its card holds.

use spacetimedb::{reducer, ReducerContext, ScheduleAt, Table, Timestamp};
use std::collections::{BTreeMap, BTreeSet};

use crate::cards::cards as _;
use crate::cards::{insert_card_row, Card, LAYER_INVENTORY};
use crate::definitions::{
  self, AspectId, CardDefinition, Duration as RecipeDuration, Entity, ProductTarget, RecipeDef,
  RecipeType,
};

// ─── Tables ──────────────────────────────────────────────────────────────────

/// One in-progress action. `card_id` is the actor — the card "running"
/// the recipe. `macro_zone` mirrors the actor's macro_zone so clients can
/// subscribe by their own `player_id` and see actions in their inventory.
#[spacetimedb::table(accessor = actions, public)]
#[derive(Debug, Clone)]
pub struct Action {
  #[primary_key]
  #[auto_inc]
  pub action_id: u32,
  /// Actor card_id (slot 1 of the matched recipe).
  #[index(btree)]
  pub card_id: u32,
  /// Recipe registry index (`RecipeDef.index`).
  pub recipe: u32,
  /// Owner of the actor card.
  #[index(btree)]
  pub owner_id: u32,
  /// Mirrors actor card's `layer`.
  pub layer: u8,
  /// Mirrors actor card's `macro_zone`. Subscription discriminator.
  #[index(btree)]
  pub macro_zone: u32,
  /// Unix seconds when this action will complete (`ctx.timestamp` at
  /// start + recipe duration).
  pub end: u32,
  /// Nibble-packed participant counts: `[up_length:u4][down_length:u4]`
  /// (bits 7..4 / bits 3..0). Both counts include the actor. For
  /// `TopStack` the actor and all slot fillers are in `up_length`;
  /// `down_length` is zero. For `BottomStack` the reverse. `OnCreate`
  /// always has `up_length = 1, down_length = 0`. Unpack with:
  /// `up = (participants >> 4) & 0x0F`, `down = participants & 0x0F`.
  pub participants: u8,
}

/// Scheduled trigger for `complete_action`. Private — clients have no
/// reason to subscribe; they read `Action.end` instead.
#[spacetimedb::table(accessor = action_scheduler, scheduled(complete_action))]
#[derive(Debug, Clone)]
pub struct ActionScheduler {
  #[primary_key]
  #[auto_inc]
  pub scheduled_id: u64,
  pub scheduled_at: ScheduleAt,
  #[index(btree)]
  pub action_id: u32,
}

/// Claim record. Each card claimed by an action gets one row keyed by
/// `card_id`. A card can be held by at most one action at a time (the PK
/// enforces this). On cancel/complete, all rows for the action are
/// deleted via the `action_id` btree index.
#[spacetimedb::table(accessor = card_holds)]
#[derive(Debug, Clone)]
pub struct CardHold {
  #[primary_key]
  pub card_id: u32,
  #[index(btree)]
  pub action_id: u32,
}
// ─── Participant packing ─────────────────────────────────────────────────────

/// Maximum count packable into either nibble of `Action.participants`.
pub const MAX_PARTICIPANT_LENGTH: u8 = 0xF;

/// Pack `(up_length, down_length)` into the `participants` u8.
pub fn pack_participants(up_length: u8, down_length: u8) -> u8 {
  ((up_length & 0x0F) << 4) | (down_length & 0x0F)
}

/// Inverse of `pack_participants`. Returns `(up_length, down_length)`.
pub fn unpack_participants(packed: u8) -> (u8, u8) {
  ((packed >> 4) & 0x0F, packed & 0x0F)
}

// ─── Time helper ─────────────────────────────────────────────────────────────

fn current_seconds(ctx: &ReducerContext) -> Result<u32, String> {
  let micros = ctx.timestamp.to_micros_since_unix_epoch();
  if micros < 0 {
    return Err("ReducerContext timestamp is before Unix epoch".to_string());
  }
  let secs = (micros / 1_000_000) as u64;
  u32::try_from(secs).map_err(|_| "ReducerContext timestamp exceeds u32 seconds range".to_string())
}

// ─── Action lifecycle helpers ────────────────────────────────────────────────

/// Single chokepoint for action removal. Releases card holds, deletes the
/// scheduler row, and deletes the action row. Every cancel and complete
/// path goes through here.
fn delete_action_rows(ctx: &ReducerContext, action_id: u32) {
  release_holds_for_action(ctx, action_id);
  let scheduler_ids: Vec<u64> = ctx
    .db
    .action_scheduler()
    .action_id()
    .filter(&action_id)
    .map(|s| s.scheduled_id)
    .collect();
  for id in scheduler_ids {
    ctx.db.action_scheduler().scheduled_id().delete(&id);
  }
  ctx.db.actions().action_id().delete(&action_id);
}

/// Insert a CardHold per claimed card_id, all keyed to `action_id`. If a
/// stale row exists (shouldn't, but defensive against reentrancy), it's
/// deleted first so the insert can't collide.
fn claim_cards(ctx: &ReducerContext, action_id: u32, card_ids: &[u32]) {
  for &card_id in card_ids {
    ctx.db.card_holds().card_id().delete(&card_id);
    ctx.db.card_holds().insert(CardHold { card_id, action_id });
  }
}

/// Wipe every CardHold belonging to `action_id`. O(claims) via the
/// `action_id` btree index.
fn release_holds_for_action(ctx: &ReducerContext, action_id: u32) {
  let card_ids: Vec<u32> = ctx
    .db
    .card_holds()
    .action_id()
    .filter(&action_id)
    .map(|h| h.card_id)
    .collect();
  for id in card_ids {
    ctx.db.card_holds().card_id().delete(&id);
  }
}

// ─── Entity matching ─────────────────────────────────────────────────────────

/// Decode a card to its definition. Returns `Ok(None)` if the card has no
/// registered definition (sentinel or unregistered packed_definition);
/// `Err` if the registry itself failed to build.
fn card_def_for(card: &Card) -> Result<Option<&'static CardDefinition>, String> {
  definitions::decode_definition(card.packed_definition)
}

/// Does `entity` match the given card definition?
///
/// `Card` and `Aspect` are leaf checks. `And` / `Or` recurse. `WeightedOr`
/// is a product-side construct, but we treat it as a non-weighted `Or`
/// for slot-matching pragmatics — a slot using `WeightedOr` accepts
/// either alternative, the weights only matter when generating outputs.
fn entity_matches(entity: &Entity, card_def: &CardDefinition) -> bool {
  match entity {
    Entity::Card(name) => card_def.key == *name,
    Entity::Aspect(aspect_id, min) => card_def
      .aspects
      .iter()
      .any(|(aid, value)| aid == aspect_id && value >= min),
    Entity::And(a, b) => entity_matches(a, card_def) && entity_matches(b, card_def),
    Entity::Or(a, b) => entity_matches(a, card_def) || entity_matches(b, card_def),
    Entity::WeightedOr { a, b, .. } => entity_matches(a, card_def) || entity_matches(b, card_def),
  }
}

/// Resolve a recipe's `Duration` against an aspect pool — used both at
/// start (to compute `Action.end`) and during conditional-duration
/// evaluation. The aspect pool is the union of `(aspect_id, value)` over
/// all participating cards.
fn resolve_duration(duration: &RecipeDuration, aspect_pool: &BTreeMap<AspectId, i32>) -> u32 {
  match duration {
    RecipeDuration::Fixed(secs) => *secs,
    RecipeDuration::Conditional { cases, fallback } => {
      for (secs, cond) in cases {
        if pool_satisfies(cond, aspect_pool) {
          return *secs;
        }
      }
      *fallback
    }
  }
}

/// Whether the aspect pool satisfies a condition entity. Used by
/// `resolve_duration`. `Card` entities don't apply to a pool of aspects
/// — they're treated as not satisfied.
fn pool_satisfies(entity: &Entity, pool: &BTreeMap<AspectId, i32>) -> bool {
  match entity {
    Entity::Card(_) => false,
    Entity::Aspect(aspect_id, min) => pool.get(aspect_id).map_or(false, |v| v >= min),
    Entity::And(a, b) => pool_satisfies(a, pool) && pool_satisfies(b, pool),
    Entity::Or(a, b) | Entity::WeightedOr { a, b, .. } => {
      pool_satisfies(a, pool) || pool_satisfies(b, pool)
    }
  }
}

/// Build the aspect pool from a slice of card definitions. Aspect values
/// from multiple cards are summed.
fn aspect_pool(defs: &[&CardDefinition]) -> BTreeMap<AspectId, i32> {
  let mut pool: BTreeMap<AspectId, i32> = BTreeMap::new();
  for def in defs {
    for (aid, value) in &def.aspects {
      *pool.entry(*aid).or_insert(0) += value;
    }
  }
  pool
}

// ─── Stack matching ──────────────────────────────────────────────────────────

/// Outcome of fitting a recipe to a stack. Carries everything needed to
/// start the action.
struct MatchResult {
  /// Actor card_id (slot 1 in the recipe).
  actor_card_id: u32,
  /// Position of the actor in the chain (0 = root, 1 = first stack_up
  /// element, etc.).
  actor_pos: usize,
  /// Card_ids that the action will claim — actor + slot fillers + root
  /// (if the recipe declares one).
  claimed: Vec<u32>,
  /// Aspect pool for duration resolution.
  pool: BTreeMap<AspectId, i32>,
}

/// Attempt to fit `recipe` against a chain of cards. The chain is
/// `[root, branch[0], branch[1], …]` for stack recipes. `recipe.slots` is
/// 1-indexed; slot 1 is the actor. The matcher slides the actor's
/// position along the chain (starting at chain[1] for rooted recipes,
/// chain[0] otherwise) and accepts the first position where every slot
/// fills against the next-N cards.
fn try_match_stack(
  recipe: &RecipeDef,
  chain: &[Card],
  defs: &[Option<&'static CardDefinition>],
) -> Option<MatchResult> {
  if recipe.slots.is_empty() {
    return None;
  }

  // Earliest position the actor can sit at. If the recipe pins a `root`
  // entity, the chain must have a root that matches and the actor sits
  // at chain[1]+. Otherwise the actor can sit at chain[0]+.
  let min_actor_pos = if recipe.root.is_some() { 1 } else { 0 };

  if let Some(root_entity) = &recipe.root {
    let root_def = defs.first().and_then(|d| *d)?;
    if !entity_matches(root_entity, root_def) {
      return None;
    }
  }

  for actor_pos in min_actor_pos..chain.len() {
    if actor_pos + recipe.slots.len() > chain.len() {
      break;
    }
    let mut all_match = true;
    for (slot_idx, slot_entity) in recipe.slots.iter().enumerate() {
      let chain_idx = actor_pos + slot_idx;
      let Some(def) = defs[chain_idx] else {
        all_match = false;
        break;
      };
      if !entity_matches(slot_entity, def) {
        all_match = false;
        break;
      }
    }
    if !all_match {
      continue;
    }

    // Build the claim window: actor + slot range, plus root if rooted.
    let mut claimed: Vec<u32> = Vec::new();
    if recipe.root.is_some() {
      claimed.push(chain[0].card_id);
    }
    for i in actor_pos..(actor_pos + recipe.slots.len()) {
      let id = chain[i].card_id;
      if !claimed.contains(&id) {
        claimed.push(id);
      }
    }

    // Aspect pool for duration: every claimed card's defs.
    let claim_defs: Vec<&CardDefinition> = claimed
      .iter()
      .filter_map(|id| {
        chain
          .iter()
          .position(|c| c.card_id == *id)
          .and_then(|i| defs[i])
      })
      .collect();
    let pool = aspect_pool(&claim_defs);

    return Some(MatchResult {
      actor_card_id: chain[actor_pos].card_id,
      actor_pos,
      claimed,
      pool,
    });
  }
  None
}

/// Build a chain of `Card` rows from card_ids. Returns `Err` if any
/// card_id can't be resolved.
fn fetch_cards(ctx: &ReducerContext, card_ids: &[u32]) -> Result<Vec<Card>, String> {
  let mut chain: Vec<Card> = Vec::with_capacity(card_ids.len());
  for &id in card_ids {
    let card = ctx
      .db
      .cards()
      .card_id()
      .find(&id)
      .ok_or_else(|| format!("card {} not found", id))?;
    chain.push(card);
  }
  Ok(chain)
}

/// Decode each card in a chain. Cards without a registered definition
/// produce `None`; the matcher treats those as never matching.
fn decode_chain(chain: &[Card]) -> Result<Vec<Option<&'static CardDefinition>>, String> {
  chain.iter().map(card_def_for).collect()
}

// ─── Public entry points ─────────────────────────────────────────────────────

/// Try to start a `TopStack` action against this stack. Iterates the
/// registered top-stack recipes in registry-declaration order; first
/// match wins.
///
/// Returns `Ok(Some(action_id))` on success, `Ok(None)` if no recipe
/// matched, `Err` on a registry-build failure or a card-resolve failure.
///
/// `chain_card_ids` is `[root, stack_up[0], stack_up[1], …]` — the
/// caller assembles this from the submitted [`crate::cards::InventoryStack`].
pub fn try_start_top_stack_action(
  ctx: &ReducerContext,
  chain_card_ids: &[u32],
  owner_id: u32,
) -> Result<Option<u32>, String> {
  try_start_stack_action(ctx, chain_card_ids, owner_id, RecipeType::TopStack)
}

/// Same as [`try_start_top_stack_action`] but for the bottom branch.
/// `chain_card_ids` is `[root, stack_down[0], stack_down[1], …]`.
pub fn try_start_bottom_stack_action(
  ctx: &ReducerContext,
  chain_card_ids: &[u32],
  owner_id: u32,
) -> Result<Option<u32>, String> {
  try_start_stack_action(ctx, chain_card_ids, owner_id, RecipeType::BottomStack)
}

fn try_start_stack_action(
  ctx: &ReducerContext,
  chain_card_ids: &[u32],
  owner_id: u32,
  recipe_type: RecipeType,
) -> Result<Option<u32>, String> {
  if chain_card_ids.is_empty() {
    return Ok(None);
  }
  let chain = fetch_cards(ctx, chain_card_ids)?;
  let defs = decode_chain(&chain)?;
  let held = held_card_set(ctx);

  for recipe in definitions::recipes_of_type(recipe_type)? {
    let Some(result) = try_match_stack(recipe, &chain, &defs) else { continue };
    // Skip if any claimed card is already held by another action.
    if result.claimed.iter().any(|id| held.contains(id)) {
      continue;
    }
    let action_id = start_action(ctx, recipe, &chain, &result, owner_id)?;
    return Ok(Some(action_id));
  }
  Ok(None)
}

/// Try to start an `OnCreate` action for a freshly-created card. The
/// recipe's `root` entity is matched against the card itself. First match
/// wins.
pub fn try_start_on_create_action(
  ctx: &ReducerContext,
  card_id: u32,
) -> Result<Option<u32>, String> {
  let card = ctx
    .db
    .cards()
    .card_id()
    .find(&card_id)
    .ok_or_else(|| format!("card {} not found", card_id))?;
  let Some(card_def) = card_def_for(&card)? else {
    return Ok(None);
  };

  let held = held_card_set(ctx);
  if held.contains(&card_id) {
    // Already participating in an action — don't double-start.
    return Ok(None);
  }

  for recipe in definitions::recipes_of_type(RecipeType::OnCreate)? {
    let Some(root_entity) = &recipe.root else { continue };
    if !entity_matches(root_entity, card_def) {
      continue;
    }

    // OnCreate uses the card itself as both root and actor. The single
    // claim is the card. Reagent index 0 references the root (= the
    // card); higher indices are not meaningful for OnCreate (no slots).
    let pool = aspect_pool(&[card_def]);
    let result = MatchResult {
      actor_card_id: card_id,
      actor_pos: 0,
      claimed: vec![card_id],
      pool,
    };
    let action_id = start_action(ctx, recipe, std::slice::from_ref(&card), &result, card.owner_id)?;
    return Ok(Some(action_id));
  }
  Ok(None)
}

/// Cancel any actions whose claim window includes any card in
/// `card_ids`. Called by the trigger path (`submit_inventory_stacks`)
/// for every card in a submitted stack — if a card is now in a
/// different stack composition than when its action started, the
/// action's claim is structurally disturbed and the action is cancelled.
///
/// Returns the number of actions cancelled.
pub fn cancel_actions_for_cards(ctx: &ReducerContext, card_ids: &[u32]) -> u32 {
  let mut to_cancel: BTreeSet<u32> = BTreeSet::new();
  for &card_id in card_ids {
    if let Some(hold) = ctx.db.card_holds().card_id().find(&card_id) {
      to_cancel.insert(hold.action_id);
    }
  }
  let count = to_cancel.len() as u32;
  for action_id in to_cancel {
    delete_action_rows(ctx, action_id);
  }
  count
}

// ─── Action start ────────────────────────────────────────────────────────────

/// Insert an Action row, schedule its completion, and claim every card
/// in the match window. Returns the new `action_id`.
fn start_action(
  ctx: &ReducerContext,
  recipe: &RecipeDef,
  chain: &[Card],
  result: &MatchResult,
  owner_id: u32,
) -> Result<u32, String> {
  let actor = chain.get(result.actor_pos).ok_or_else(|| {
    format!(
      "actor_pos {} out of range for chain length {}",
      result.actor_pos,
      chain.len()
    )
  })?;
  let duration = resolve_duration(&recipe.duration, &result.pool);
  let now = current_seconds(ctx)?;
  let end = now.saturating_add(duration);

  let complete_at = Timestamp::from_micros_since_unix_epoch(
    ctx
      .timestamp
      .to_micros_since_unix_epoch()
      .saturating_add(duration as i64 * 1_000_000),
  );

  let slot_count = recipe.slots.len();
  if slot_count > MAX_PARTICIPANT_LENGTH as usize {
    return Err(format!(
      "recipe {:?} has {} slots; exceeds nibble max ({})",
      recipe.id, slot_count, MAX_PARTICIPANT_LENGTH,
    ));
  }
  let slot_count = slot_count as u8;
  let participants = match recipe.recipe_type {
    RecipeType::TopStack => pack_participants(slot_count, 0),
    RecipeType::BottomStack => pack_participants(0, slot_count),
    RecipeType::OnCreate => pack_participants(1, 0),
  };

  let inserted = ctx.db.actions().insert(Action {
    action_id: 0,
    card_id: result.actor_card_id,
    recipe: recipe.index,
    owner_id,
    layer: actor.layer,
    macro_zone: actor.macro_zone,
    end,
    participants,
  });
  ctx.db.action_scheduler().insert(ActionScheduler {
    scheduled_id: 0,
    scheduled_at: ScheduleAt::Time(complete_at),
    action_id: inserted.action_id,
  });
  claim_cards(ctx, inserted.action_id, &result.claimed);
  Ok(inserted.action_id)
}

fn held_card_set(ctx: &ReducerContext) -> BTreeSet<u32> {
  ctx.db.card_holds().iter().map(|h| h.card_id).collect()
}

// ─── Reducers ────────────────────────────────────────────────────────────────

/// Scheduled completion handler. Triggered by SpacetimeDB when the
/// scheduler row's `scheduled_at` arrives. Generates products, consumes
/// reagents, then tears down the action.
///
/// **Defended against client-spoofed invocation.** Although the
/// `#[reducer]` attribute makes this technically callable from a client,
/// the entry guard rejects any call where:
///
/// - the scheduler row passed in doesn't exist in the table (a fabricated
///   row, or one for an already-completed action), or
/// - the action's `end` is in the future (a legitimate scheduled fire
///   happens at exactly `end`; a future end means the caller is
///   accelerating).
///
/// Together these mean a client can at best replay a completion that's
/// already due — no acceleration, no completion of someone else's
/// pending action.
#[reducer]
pub fn complete_action(ctx: &ReducerContext, scheduler: ActionScheduler) -> Result<(), String> {
  let action_id = scheduler.action_id;

  // Guard 1: scheduler row must actually exist in the table. SpacetimeDB
  // deletes scheduled rows after the reducer completes, so during a
  // legitimate fire it's still present. A spoofed call with a fabricated
  // row would fail this lookup.
  let real_scheduler = ctx
    .db
    .action_scheduler()
    .scheduled_id()
    .find(&scheduler.scheduled_id)
    .ok_or_else(|| {
      format!(
        "complete_action: scheduler row {} not found (spoofed or already completed)",
        scheduler.scheduled_id
      )
    })?;
  if real_scheduler.action_id != action_id {
    return Err(format!(
      "complete_action: scheduler {} action_id mismatch ({} vs {})",
      scheduler.scheduled_id, real_scheduler.action_id, action_id,
    ));
  }

  let action = ctx
    .db
    .actions()
    .action_id()
    .find(&action_id)
    .ok_or_else(|| format!("complete_action: action {} not found", action_id))?;

  // Guard 2: action's end-time must have passed. A legitimate
  // SpacetimeDB-initiated fire happens at exactly `action.end`; anything
  // strictly earlier is acceleration and we refuse.
  let now = current_seconds(ctx)?;
  if action.end > now {
    return Err(format!(
      "complete_action: action {} not yet due (end={}, now={})",
      action_id, action.end, now
    ));
  }

  let recipe = definitions::recipe(action.recipe)?
    .ok_or_else(|| format!("complete_action: recipe {} not in registry", action.recipe))?;

  // Hold rows for this action enumerate the claimed cards.
  let claimed_ids: Vec<u32> = ctx
    .db
    .card_holds()
    .action_id()
    .filter(&action_id)
    .map(|h| h.card_id)
    .collect();
  let claimed_cards = fetch_cards(ctx, &claimed_ids)?;

  // Find the actor inside the claim list (it always has a hold). Used
  // for product-target resolution.
  let actor = claimed_cards
    .iter()
    .find(|c| c.card_id == action.card_id)
    .cloned()
    .ok_or_else(|| {
      format!(
        "complete_action: actor card {} not in action's claim window",
        action.card_id
      )
    })?;

  // Generate products before deleting reagents. The RNG seed is the
  // scheduler id so the outcome is reproducible per-action.
  if !recipe.products.is_empty() {
    let mut rng_state: u32 = scheduler.scheduled_id as u32;
    generate_products(ctx, recipe, &actor, &claimed_cards, &mut rng_state)?;
  }

  // Consume reagents. Reagent indices are 1-based against the recipe's
  // slots (slot 1 = actor, slot 2 = next card outward, …); index 0
  // refers to the chain root.
  //
  // Important: we **don't** index into `claimed_cards` to resolve a slot
  // position. The vec was built from
  // `card_holds().action_id().filter()`, whose secondary iteration
  // order is PK (card_id), not the original claim/insertion order. So
  // `claimed_cards[0]` is the lowest-card_id claimed card, not
  // necessarily slot 1.
  //
  // Reagent 1 (the actor) is `action.card_id` — known unambiguously.
  // Reagent 0 in OnCreate is also the actor. Reagent 0 in a rooted
  // stack recipe is the chain root, which we recover by scanning
  // `claimed_cards` for the one whose def matches `recipe.root`.
  // Reagents past slot 1 (`n >= 2`) require slot-position info that
  // isn't currently preserved on `CardHold` — those are skipped with a
  // log warning.
  for &reagent_idx in &recipe.reagents {
    let card_id = match reagent_idx {
      0 => {
        match recipe.recipe_type {
          RecipeType::OnCreate => action.card_id,
          RecipeType::TopStack | RecipeType::BottomStack => {
            let Some(root_entity) = &recipe.root else {
              // Reagent 0 in a non-rooted stack recipe is currently a
              // no-op (the matcher doesn't claim chain[0] for
              // non-rooted recipes). Documented gotcha; needs the
              // matcher to also claim the chain root when reagent 0 is
              // listed before this branch can do anything useful.
              continue;
            };
            // Find the claimed card whose def matches the recipe's
            // root entity. There should be exactly one (the chain
            // root).
            let mut found: Option<u32> = None;
            for c in &claimed_cards {
              if let Some(def) = card_def_for(c)? {
                if entity_matches(root_entity, def) {
                  found = Some(c.card_id);
                  break;
                }
              }
            }
            match found {
              Some(id) => id,
              None => continue,
            }
          }
        }
      }
      1 => {
        // Slot 1 is always the actor — known unambiguously regardless
        // of CardHold iteration order.
        action.card_id
      }
      _n => {
        // TODO: reagents N >= 2 reference slot positions that aren't
        // recoverable from `card_holds().action_id().filter()` alone
        // (the iteration order is PK, not insertion). Fix by adding a
        // `slot_index: u8` field to `CardHold` so we can look up the
        // claim for a specific slot, or by storing the ordered claim
        // list on the `Action` row. None of the recipes in
        // `data/recipes/01.json` use this — every reagent there is 0
        // or 1 — so this is forward-looking, not currently broken.
        continue;
      }
    };

    // Cancel any *other* action holding this card before we delete it,
    // so we don't strand a CardHold pointing at a vanished card. (The
    // current action's hold is fine — it'll be released when
    // `delete_action_rows` runs at the end.)
    if let Some(hold) = ctx.db.card_holds().card_id().find(&card_id) {
      if hold.action_id != action_id {
        delete_action_rows(ctx, hold.action_id);
      }
    }
    ctx.db.cards().card_id().delete(&card_id);
  }

  delete_action_rows(ctx, action_id);
  Ok(())
}

// ─── Product generation ──────────────────────────────────────────────────────

/// Where one product card row will be inserted.
enum ProductDestination {
  /// Inventory panel held by this player. The product card's
  /// `macro_zone` will be set to `panel_player_id` (subscription
  /// scoping). Today we also set `owner_id` to the same player; richer
  /// ownership (e.g. "produce into someone else's panel but I own it")
  /// belongs in a future widened destination type.
  Panel { panel_player_id: u32 },
}

fn generate_products(
  ctx: &ReducerContext,
  recipe: &RecipeDef,
  actor: &Card,
  claimed_cards: &[Card],
  rng: &mut u32,
) -> Result<(), String> {
  for group in &recipe.products {
    let dest = resolve_product_destination(&group.target, actor, claimed_cards);
    for entity in &group.entities {
      generate_entity_products(ctx, entity, &dest, rng)?;
    }
  }
  Ok(())
}

fn resolve_product_destination(
  target: &ProductTarget,
  actor: &Card,
  claimed_cards: &[Card],
) -> ProductDestination {
  // The destination panel is identified by `macro_zone` — the player
  // whose inventory holds the relevant card — *not* by `owner_id`. For
  // inventory cards `macro_zone == panel_player_id` by definition.
  match target {
    ProductTarget::ActorPanel => ProductDestination::Panel {
      panel_player_id: actor.macro_zone,
    },
    ProductTarget::RootPanel => {
      // Root is claimed[0] when the recipe declared a root; otherwise
      // there's no root and we fall back to the actor's panel.
      let root = claimed_cards.first().unwrap_or(actor);
      ProductDestination::Panel {
        panel_player_id: root.macro_zone,
      }
    }
  }
}

fn generate_entity_products(
  ctx: &ReducerContext,
  entity: &Entity,
  dest: &ProductDestination,
  rng: &mut u32,
) -> Result<(), String> {
  match entity {
    Entity::Card(name) => {
      // Bare name → first matching packed_definition across all types.
      // Card keys aren't guaranteed globally unique; if a recipe needs
      // a specific (type, key), the JSON should use a `"type/key"` form
      // and the parser should keep the prefix — currently
      // `parse_entity` strips it down to `Entity::Card(<bare name>)`.
      // TODO: extend `parse_entity` to recognise `"type/key"` strings
      // and produce a fully-qualified `Entity::Card`.
      match definitions::find_packed_by_key(name)? {
        Some(packed_definition) => {
          insert_product(ctx, packed_definition, dest)?;
        }
        None => {
          // Quietly drop unresolved product names rather than failing
          // the whole completion. A logging hook would help here.
        }
      }
    }
    Entity::And(a, b) => {
      generate_entity_products(ctx, a, dest, rng)?;
      generate_entity_products(ctx, b, dest, rng)?;
    }
    Entity::Or(a, _b) => {
      // Bare `Or` in products: pick `a` deterministically. This shape
      // is rare in product lists; the typical product OR is weighted.
      // The `_b` arm is intentionally never selected — products
      // expressing alternatives should use `WeightedOr`.
      generate_entity_products(ctx, a, dest, rng)?;
    }
    Entity::WeightedOr {
      a,
      b,
      weight_a,
      weight_b,
    } => {
      *rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
      let total = weight_a.saturating_add(*weight_b);
      let pick_a = total == 0 || (*rng % total) < *weight_a;
      if pick_a {
        generate_entity_products(ctx, a, dest, rng)?;
      } else {
        generate_entity_products(ctx, b, dest, rng)?;
      }
    }
    Entity::Aspect(_, _) => {
      // An aspect check is a slot-side construct; it doesn't describe
      // an output card. Silently skip.
    }
  }
  Ok(())
}

fn insert_product(
  ctx: &ReducerContext,
  packed_definition: u16,
  dest: &ProductDestination,
) -> Result<(), String> {
  match dest {
    ProductDestination::Panel { panel_player_id } => {
      // Inventory product: the new card sits in `panel_player_id`'s
      // inventory and is owned by them. `insert_card_row` itself
      // triggers the on_create recipe check, which is how
      // completion-chains-into-another-recipe (e.g. `corpus_stacked` →
      // `fatigue` → `corpus`) work without any extra plumbing here.
      insert_card_row(
        ctx,
        LAYER_INVENTORY,
        *panel_player_id,
        *panel_player_id,
        packed_definition,
      )?;
    }
  }
  Ok(())
}
