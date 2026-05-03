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
//! - [`process_top_branch`] — caller passes the submitted root +
//!   up-branch; we iterate every potential actor along the chain,
//!   evaluate `TopStack` recipes against each actor's visible window,
//!   and apply the upgrade rules below.
//! - [`process_bottom_branch`] — same shape for the down-branch.
//! - [`try_start_on_create_action`] — caller passes a freshly-inserted
//!   card; we try every `OnCreate` recipe whose `root` matches that card.
//!
//! # Visible chain & upgrade rules
//!
//! For each potential actor in the submitted branch chain, we build a
//! **visible chain** — the actor plus cards extending outward (toward
//! higher branch indices) that are either *free* (no `CardHold`) or
//! *claimed by the actor's own current action*. The walk stops at the
//! first card claimed by some other action.
//!
//! With the actor's current action (`current`, may be `None`) and the
//! best-scoring recipe over the visible chain (`best`, may be `None`):
//!
//! ```text
//! (None,    None)    → nothing
//! (Some(a), None)    → cancel a
//! (None,    Some(r)) → start r
//! (Some(a), Some(r)) →
//!     same recipe AND slot fillers unchanged → keep a running
//!     otherwise → cancel a, start r
//! ```
//!
//! Slot fillers are **strict** — any card swap, reorder, or removal in
//! the slot window cancels and (if a recipe still matches) restarts.
//! The chain root is **fluid** — it isn't held in `CardHold` and isn't
//! tracked on the `Action` row. The recipe's `root` entity is just a
//! pre-condition the matcher re-checks on every submission; if the
//! chain root drifted but still satisfies `root`, the action keeps
//! running unchanged. If the new root no longer matches, the matcher
//! returns `None` and the action is cancelled.
//!
//! Not holding the root is what lets multiple recipes share one — e.g.
//! `[attack, sword]` over a top branch and `[heal, anima]` over the
//! bottom can both be rooted on the same `human` card concurrently.
//! Holding the root would have made that single card a contention
//! point and forced the recipes to be mutually exclusive.
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
  /// Actor card_id (slot 1 of the matched recipe). Set at start, never
  /// changes for the lifetime of the action.
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

// Per-leaf match weights (used by `entity_match_weight`). Higher = more
// specific. Tiers:
//
//   Card (exact key)        : 4
//   Aspect (named, ≥ value) : 3
//   Type (card_type)        : 2
//   Any (wildcard)          : 1
//
// A composite's weight is built up from its children:
//
//   And(A, B) — both must match; weight = weight(A) + weight(B)
//   Or(A, B) | WeightedOr   — weight of whichever branch satisfied
//
// For a non-match the weight is 0 (treated as "didn't match" by callers
// that only care about a yes/no answer).
const ENTITY_WEIGHT_CARD: u32 = 4;
const ENTITY_WEIGHT_ASPECT: u32 = 3;
const ENTITY_WEIGHT_TYPE: u32 = 2;
const ENTITY_WEIGHT_ANY: u32 = 1;

/// Score how specifically `entity` matches `card_def`. `0` means no match;
/// any positive value indicates a match, with higher = more specific. See
/// the constants above for the per-leaf weight scale.
///
/// Used both for "does this fit?" yes/no checks (caller compares `> 0`)
/// and for the priority weighting that picks the best recipe across a
/// stack (caller sums slot weights, plus tile/root tier weights).
fn entity_match_weight(entity: &Entity, card_def: &CardDefinition) -> u32 {
  match entity {
    Entity::Card(name) => {
      if card_def.key == *name {
        ENTITY_WEIGHT_CARD
      } else {
        0
      }
    }
    Entity::Aspect(aspect_id, min) => {
      let matches = card_def
        .aspects
        .iter()
        .any(|(aid, value)| aid == aspect_id && value >= min);
      if matches {
        ENTITY_WEIGHT_ASPECT
      } else {
        0
      }
    }
    Entity::Type(type_id) => {
      if card_def.card_type == *type_id {
        ENTITY_WEIGHT_TYPE
      } else {
        0
      }
    }
    Entity::Any => ENTITY_WEIGHT_ANY,
    Entity::And(a, b) => {
      let wa = entity_match_weight(a, card_def);
      let wb = entity_match_weight(b, card_def);
      // AND requires both children to match. Sum gives a slot using
      // `[corpus, ["labor", 1]]` (key + aspect, both required) a
      // weight of 4 + 3 = 7 — strictly more than either alone.
      if wa > 0 && wb > 0 {
        wa + wb
      } else {
        0
      }
    }
    Entity::Or(a, b) => {
      // Take the weight of whichever branch satisfied (first if both).
      let wa = entity_match_weight(a, card_def);
      if wa > 0 {
        wa
      } else {
        entity_match_weight(b, card_def)
      }
    }
    Entity::WeightedOr { a, b, .. } => {
      // For slot-side use the weight of the satisfying branch — same
      // shape as `Or`. The weights inside `WeightedOr` are for product
      // selection at completion, not for slot specificity here.
      let wa = entity_match_weight(a, card_def);
      if wa > 0 {
        wa
      } else {
        entity_match_weight(b, card_def)
      }
    }
  }
}

/// Thin boolean wrapper over `entity_match_weight` for callers that don't
/// care about specificity.
fn entity_matches(entity: &Entity, card_def: &CardDefinition) -> bool {
  entity_match_weight(entity, card_def) > 0
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
/// `resolve_duration`. `Card` and `Type` entities are card-shape checks
/// that don't apply to a pool of aspects — treated as not satisfied.
/// `Any` always satisfies (the trivial condition).
fn pool_satisfies(entity: &Entity, pool: &BTreeMap<AspectId, i32>) -> bool {
  match entity {
    Entity::Card(_) | Entity::Type(_) => false,
    Entity::Any => true,
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

// ─── Recipe scoring ──────────────────────────────────────────────────────────

/// Lexicographically-compared priority for a successful recipe match.
///
/// Field order **is** the comparison order: `tile_weight` outranks
/// `root_weight` outranks `slot_weight`. Within a tier, the value is the
/// `entity_match_weight` of how that condition was satisfied — so
/// `tile: "forest"` (Card → 4) outranks `tile: ["wood", 1]` (Aspect → 3)
/// in the tile tier without ever consulting root or slots.
///
/// Recipes with no `tile` field score `tile_weight = 0`; same for `root`.
/// `slot_weight` is the sum of per-slot weights, so a recipe with N
/// card-key slots scores 4N — but no number of slot weights can defeat a
/// recipe whose tile/root tier is non-zero, because comparison stops at
/// the first non-equal tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
struct MatchWeight {
  tile_weight: u32,
  root_weight: u32,
  slot_weight: u32,
}

/// Outcome of scoring a recipe against an actor candidate. Carries
/// everything `start_action` needs.
struct ActorMatch {
  weight: MatchWeight,
  /// Card_ids the action will claim — actor + slot fillers, in chain
  /// order. The chain root is **not** included even when the recipe
  /// has a `root` entity: holding it would block other recipes from
  /// rooting on the same card, e.g. `[attack, sword] + human` and
  /// `[heal, anima] + human` running concurrently. The matcher
  /// re-checks `recipe.root` against the current chain root on every
  /// upgrade pass, so the root drifting away cancels the action
  /// without needing a `CardHold` on it.
  claimed: Vec<u32>,
  /// Aspect pool for duration resolution.
  pool: BTreeMap<AspectId, i32>,
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

/// Score `recipe` for an actor at `branch_chain[actor_idx]` over the
/// visible window `branch_chain[actor_idx..visible_end]`. The chain
/// root is always `branch_chain[0]` (the submitted root) regardless of
/// where the actor sits — for `top_stack` and `bottom_stack` recipes
/// alike. Returns `None` if the slot list doesn't fit, any required
/// entity fails to match, or any decode fails.
fn score_recipe_for_actor(
  recipe: &RecipeDef,
  branch_chain: &[Card],
  branch_chain_defs: &[Option<&'static CardDefinition>],
  actor_idx: usize,
  visible_end: usize,
) -> Option<ActorMatch> {
  let slot_count = recipe.slots.len();
  if slot_count == 0 {
    return None;
  }
  if actor_idx + slot_count > visible_end {
    return None;
  }

  // Rooted recipes pin the chain root at `branch_chain[0]` and the
  // actor sits *above* it — so actor_idx 0 is reserved for the root,
  // not the actor. Without this guard, a recipe with both `root` and a
  // slot list could double-match its first slot against the chain
  // root, producing a degenerate single-card claim.
  if recipe.root.is_some() && actor_idx == 0 {
    return None;
  }

  // Root tier weight — constant across actor positions because the
  // chain root is always `branch_chain[0]`. For recipes without
  // `root`, contributes 0.
  let root_weight = if let Some(root_entity) = &recipe.root {
    let def = (*branch_chain_defs.first()?)?;
    let w = entity_match_weight(root_entity, def);
    if w == 0 {
      return None;
    }
    w
  } else {
    0
  };

  // Tile tier — forward-looking. See note on tile resolution in
  // `data/recipes/AGENT.md`. Today this is always 0.
  let tile_weight = 0;

  let mut slot_weight: u32 = 0;
  for (i, slot_entity) in recipe.slots.iter().enumerate() {
    let def = branch_chain_defs[actor_idx + i]?;
    let w = entity_match_weight(slot_entity, def);
    if w == 0 {
      return None;
    }
    slot_weight += w;
  }

  // Build claim list: actor + slot fillers, in chain order. The chain
  // root is intentionally not held — see `ActorMatch.claimed` doc.
  let mut claimed: Vec<u32> = Vec::with_capacity(slot_count);
  for i in 0..slot_count {
    claimed.push(branch_chain[actor_idx + i].card_id);
  }

  // Aspect pool for duration: every claimed card's def + the chain
  // root's def for rooted recipes (the root contributes its aspects
  // to the pool even though it isn't held).
  let mut claim_defs: Vec<&CardDefinition> = claimed
    .iter()
    .filter_map(|id| {
      branch_chain
        .iter()
        .zip(branch_chain_defs.iter())
        .find_map(|(c, d)| if c.card_id == *id { *d } else { None })
    })
    .collect();
  if recipe.root.is_some() {
    if let Some(def) = branch_chain_defs.first().and_then(|d| *d) {
      if !claimed.contains(&branch_chain[0].card_id) {
        claim_defs.push(def);
      }
    }
  }
  let pool = aspect_pool(&claim_defs);

  Some(ActorMatch {
    weight: MatchWeight { tile_weight, root_weight, slot_weight },
    claimed,
    pool,
  })
}

// ─── Visible chain ───────────────────────────────────────────────────────────

/// Walk outward from `branch_chain[actor_idx]` and return the exclusive
/// end index of the visible window. A card is visible if it is *free*
/// (no `CardHold`) or *claimed by the actor's own action*. The walk
/// stops at the first card claimed by some other action, excluding it.
fn build_visible_chain(
  ctx: &ReducerContext,
  branch_chain: &[Card],
  actor_idx: usize,
  actor_action_id: Option<u32>,
) -> usize {
  let mut end = actor_idx;
  for j in actor_idx..branch_chain.len() {
    let hold_action = ctx
      .db
      .card_holds()
      .card_id()
      .find(&branch_chain[j].card_id)
      .map(|h| h.action_id);
    let visible = match (hold_action, actor_action_id) {
      (None, _) => true,
      (Some(a), Some(b)) if a == b => true,
      _ => false,
    };
    if visible {
      end = j + 1;
    } else {
      break;
    }
  }
  end
}

// ─── Strict slot-filler equality ─────────────────────────────────────────────

/// Whether the action's currently-claimed cards match the new slot
/// window `branch_chain[actor_idx..actor_idx+recipe.slots.len()]`
/// **as a set**.
///
/// Used as the strict "slot fillers haven't moved" gate before keeping
/// a same-recipe action running. The claim is exactly the slot window
/// (actor + slot fillers) — the chain root isn't held — so the
/// comparison is direct, no set subtraction needed.
///
/// Set equality (rather than positional) reflects what we can compare
/// with the data on hand — `CardHold` doesn't preserve slot index. In
/// practice the chain order is fixed by the user's stack, so a set
/// match implies a positional match for any well-formed submission.
fn slot_fillers_unchanged(
  ctx: &ReducerContext,
  action: &Action,
  recipe: &RecipeDef,
  branch_chain: &[Card],
  actor_idx: usize,
) -> bool {
  let slot_count = recipe.slots.len();
  if actor_idx + slot_count > branch_chain.len() {
    return false;
  }
  let new_set: BTreeSet<u32> = branch_chain[actor_idx..actor_idx + slot_count]
    .iter()
    .map(|c| c.card_id)
    .collect();
  let old_set: BTreeSet<u32> = ctx
    .db
    .card_holds()
    .action_id()
    .filter(&action.action_id)
    .map(|h| h.card_id)
    .collect();
  old_set == new_set
}

// ─── Public entry points ─────────────────────────────────────────────────────

/// Process the top branch of a submitted stack. `branch_chain_ids` is
/// `[root, stack_up[0], stack_up[1], …]`. For every card in the chain,
/// the matcher evaluates that card as a potential actor over its
/// visible chain, scores all `TopStack` recipes, and applies the
/// upgrade rules from the module docs.
pub fn process_top_branch(
  ctx: &ReducerContext,
  branch_chain_ids: &[u32],
  owner_id: u32,
) -> Result<(), String> {
  process_branch(ctx, branch_chain_ids, RecipeType::TopStack, owner_id)
}

/// Same as [`process_top_branch`] for the bottom branch.
/// `branch_chain_ids` is `[root, stack_down[0], stack_down[1], …]`.
pub fn process_bottom_branch(
  ctx: &ReducerContext,
  branch_chain_ids: &[u32],
  owner_id: u32,
) -> Result<(), String> {
  process_branch(ctx, branch_chain_ids, RecipeType::BottomStack, owner_id)
}

fn process_branch(
  ctx: &ReducerContext,
  branch_chain_ids: &[u32],
  recipe_type: RecipeType,
  owner_id: u32,
) -> Result<(), String> {
  if branch_chain_ids.is_empty() {
    return Ok(());
  }
  let branch_chain = fetch_cards(ctx, branch_chain_ids)?;
  let branch_chain_defs = decode_chain(&branch_chain)?;

  for actor_idx in 0..branch_chain.len() {
    process_actor_candidate(
      ctx,
      &branch_chain,
      &branch_chain_defs,
      actor_idx,
      recipe_type,
      owner_id,
    )?;
  }
  Ok(())
}

/// Apply the upgrade decision for one actor candidate. Single source of
/// truth for the four-way table in the module docs.
fn process_actor_candidate(
  ctx: &ReducerContext,
  branch_chain: &[Card],
  branch_chain_defs: &[Option<&'static CardDefinition>],
  actor_idx: usize,
  recipe_type: RecipeType,
  owner_id: u32,
) -> Result<(), String> {
  let actor = &branch_chain[actor_idx];

  // Look up the actor's current action, if any. The actor is *us* only
  // when the held action's `card_id` equals the actor's id; otherwise
  // this card is a slot filler in someone else's action and we leave
  // it alone (its actor will reach the same conclusion when *its*
  // candidate iteration runs).
  let actor_action_id = ctx
    .db
    .card_holds()
    .card_id()
    .find(&actor.card_id)
    .map(|h| h.action_id);
  let current_action = actor_action_id.and_then(|id| ctx.db.actions().action_id().find(&id));
  if let Some(ref a) = current_action {
    if a.card_id != actor.card_id {
      return Ok(());
    }
    // Actor's current action is for a *different* branch direction. The
    // root card of a Y-stack is the actor of one branch's action and
    // also sits at chain[0] of the other branch — but the other branch
    // has no business cancelling the first branch's action. The other
    // branch's own evaluator will handle that action when it runs.
    let current_recipe = definitions::recipe(a.recipe)?;
    if let Some(cur) = current_recipe {
      if cur.recipe_type != recipe_type {
        return Ok(());
      }
    }
  }

  // Visible window [actor_idx, visible_end). For a free actor this is
  // free-or-empty cards beyond it; for an actor mid-action this also
  // includes the action's own slot fillers.
  let visible_end = build_visible_chain(ctx, branch_chain, actor_idx, actor_action_id);

  // Score every recipe of this type against the visible window. Skip
  // candidates whose claim would conflict with another action's hold.
  // (The actor's own action is not a conflict — we may keep it.)
  let mut best: Option<(&'static RecipeDef, ActorMatch)> = None;
  for recipe in definitions::recipes_of_type(recipe_type)? {
    let Some(m) = score_recipe_for_actor(
      recipe,
      branch_chain,
      branch_chain_defs,
      actor_idx,
      visible_end,
    ) else {
      continue;
    };

    let blocked = m.claimed.iter().any(|id| {
      ctx.db.card_holds().card_id().find(id).map_or(false, |h| {
        Some(h.action_id) != actor_action_id
      })
    });
    if blocked {
      continue;
    }

    match best.as_ref() {
      None => best = Some((recipe, m)),
      Some((_, b)) if m.weight > b.weight => best = Some((recipe, m)),
      _ => {}
    }
  }

  // Four-way decision. See module docs for the rules.
  match (&current_action, best) {
    (None, None) => Ok(()),
    (Some(a), None) => {
      delete_action_rows(ctx, a.action_id);
      Ok(())
    }
    (None, Some((recipe, m))) => {
      start_action(ctx, recipe, actor, branch_chain, &m, owner_id)?;
      Ok(())
    }
    (Some(a), Some((recipe, m))) => {
      if a.recipe == recipe.index && slot_fillers_unchanged(ctx, a, recipe, branch_chain, actor_idx) {
        // Same recipe, same slot fillers — keep running. The chain
        // root isn't held, so a drifted-but-still-matching root needs
        // no bookkeeping update; the matcher already validated it
        // when scoring `m` above.
        Ok(())
      } else {
        // Different recipe, or slot fillers moved — cancel and start.
        delete_action_rows(ctx, a.action_id);
        start_action(ctx, recipe, actor, branch_chain, &m, owner_id)?;
        Ok(())
      }
    }
  }
}

/// `OnCreate` matcher. The freshly-created card is both root and
/// actor; the visible chain is the card itself. Picks the highest-
/// weight `OnCreate` recipe whose `root` entity matches the card.
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

  if ctx.db.card_holds().card_id().find(&card_id).is_some() {
    // Already participating in an action — don't double-start.
    return Ok(None);
  }

  let mut best: Option<(&'static RecipeDef, ActorMatch)> = None;
  for recipe in definitions::recipes_of_type(RecipeType::OnCreate)? {
    let Some(root_entity) = &recipe.root else { continue };
    let root_w = entity_match_weight(root_entity, card_def);
    if root_w == 0 {
      continue;
    }
    let pool = aspect_pool(&[card_def]);
    let m = ActorMatch {
      weight: MatchWeight {
        tile_weight: 0,
        root_weight: root_w,
        slot_weight: 0,
      },
      claimed: vec![card_id],
      pool,
    };
    match best.as_ref() {
      None => best = Some((recipe, m)),
      Some((_, b)) if m.weight > b.weight => best = Some((recipe, m)),
      _ => {}
    }
  }
  match best {
    Some((recipe, m)) => {
      let branch_chain = vec![card.clone()];
      let action_id = start_action(ctx, recipe, &card, &branch_chain, &m, card.owner_id)?;
      Ok(Some(action_id))
    }
    None => Ok(None),
  }
}

// ─── Action start / refresh ──────────────────────────────────────────────────

/// Insert an Action row, schedule its completion, and claim every card
/// in the match window. Returns the new `action_id`.
fn start_action(
  ctx: &ReducerContext,
  recipe: &RecipeDef,
  actor: &Card,
  branch_chain: &[Card],
  m: &ActorMatch,
  owner_id: u32,
) -> Result<u32, String> {
  let duration = resolve_duration(&recipe.duration, &m.pool);
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
    card_id: actor.card_id,
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
  claim_cards(ctx, inserted.action_id, &m.claimed);
  Ok(inserted.action_id)
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

  // Defense-in-depth: re-check that every claimed card's current
  // definition still satisfies the recipe shape. The upgrade path is
  // supposed to cancel any action whose claim drifted, but a desync
  // (mutated card def, lost hold, etc.) shouldn't be able to push a
  // stale completion through. Refuse rather than produce mismatched
  // products.
  if !recipe_still_satisfies_claim(recipe, &claimed_cards)? {
    delete_action_rows(ctx, action_id);
    return Err(format!(
      "complete_action: action {} no longer satisfies recipe {:?} — refused",
      action_id, recipe.id,
    ));
  }

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
    generate_products(ctx, recipe, &actor, &mut rng_state)?;
  }

  // Consume reagents. Reagent indices are 1-based against the recipe's
  // slots (slot 1 = actor, slot 2 = next card outward, …); index `0`
  // refers to the chain root.
  //
  // For `OnCreate`, the actor *is* the chain root — `action.card_id`
  // resolves both indices `0` and `1` to the same card. For stack
  // recipes, the chain root isn't held (so multiple recipes can root
  // on it concurrently) and isn't stored on the `Action` row, so
  // reagent `0` is a no-op for stack recipes today; if a future
  // recipe needs to consume the chain root of a stack, that's where
  // chain-context-at-completion needs to come from. None of the
  // current recipes use this for stack types.
  //
  // Reagents past slot 1 (`n >= 2`) need slot-position info that
  // CardHold doesn't preserve — see TODO below.
  for &reagent_idx in &recipe.reagents {
    let card_id = match reagent_idx {
      0 => match recipe.recipe_type {
        RecipeType::OnCreate => action.card_id,
        RecipeType::TopStack | RecipeType::BottomStack => continue,
      },
      1 => {
        // Slot 1 is always the actor.
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

/// Defense-in-depth: verify the claim window is still consistent with
/// the recipe at completion time. The upgrade machinery is supposed to
/// have cancelled any drifted action long before now; this is the
/// belt-and-braces check that runs anyway.
///
/// Every claimed card must still match at least one slot entity in
/// the recipe. This isn't a strict positional check (`CardHold` doesn't
/// preserve slot index) but it catches a `packed_definition` that's
/// drifted to something the recipe wouldn't accept at any position.
///
/// The chain root isn't held — so it isn't in `claimed_cards` and
/// isn't checked here. The matcher re-validates `recipe.root` against
/// the current chain root on every upgrade pass, which is the only
/// place it can be checked (the chain root isn't recoverable from
/// server state at completion time).
fn recipe_still_satisfies_claim(
  recipe: &RecipeDef,
  claimed_cards: &[Card],
) -> Result<bool, String> {
  for c in claimed_cards {
    let Some(def) = card_def_for(c)? else {
      return Ok(false);
    };
    // OnCreate has empty `slots`; its claim is the actor / root, which
    // we instead check against `recipe.root`.
    let matches = if recipe.slots.is_empty() {
      recipe.root.as_ref().map_or(false, |r| entity_matches(r, def))
    } else {
      recipe.slots.iter().any(|e| entity_matches(e, def))
    };
    if !matches {
      return Ok(false);
    }
  }
  Ok(true)
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
  rng: &mut u32,
) -> Result<(), String> {
  for group in &recipe.products {
    let dest = resolve_product_destination(&group.target, actor);
    for entity in &group.entities {
      generate_entity_products(ctx, entity, &dest, rng)?;
    }
  }
  Ok(())
}

fn resolve_product_destination(
  target: &ProductTarget,
  actor: &Card,
) -> ProductDestination {
  // The destination panel is identified by `macro_zone` — the player
  // whose inventory holds the relevant card — *not* by `owner_id`. For
  // inventory cards `macro_zone == panel_player_id` by definition.
  //
  // `RootPanel` would ideally route to the chain root's holder, but
  // the chain root isn't held by the action and isn't recoverable
  // from server state at completion (server doesn't track inventory
  // stack composition). For the inventory POC every claimed card is
  // in the same player's panel anyway, so falling back to the actor's
  // panel is a no-op there. When world layers land and a stack can
  // span panels, the chain root will need a server-side
  // representation (likely passed at submission and snapshotted onto
  // the `Action` row).
  match target {
    ProductTarget::ActorPanel | ProductTarget::RootPanel => ProductDestination::Panel {
      panel_player_id: actor.macro_zone,
    },
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
    Entity::Aspect(_, _) | Entity::Type(_) | Entity::Any => {
      // Slot-side constructs that don't describe an output card.
      // Silently skip — useful in slot grammars, meaningless here.
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
