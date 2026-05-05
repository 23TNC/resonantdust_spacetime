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
//!   evaluate `Stack(Up)` recipes against each actor's visible window,
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
//! 2. Generates products into the configured targets (e.g.
//!    `inventory.root` / `inventory.actor`).
//! 3. Deletes the cards listed in `recipe.reagents`.
//! 4. Tears down the action, scheduler row, and all its card holds.

use spacetimedb::{reducer, ReducerContext, ScheduleAt, Table, Timestamp};
use std::collections::{BTreeMap, BTreeSet};

use crate::cards::cards as _;
use crate::cards::{insert_card_row, Card, LAYER_INVENTORY};
use crate::definitions::{
  self, AspectId, CardDefinition, Duration as RecipeDuration, Entity, ProductGroup, ProductOwner,
  ProductPlace, ProductTarget, Reagent, RecipeDef, RecipeType,
};
use crate::zones;
use crate::zones::zones as _;

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
  /// Packed recipe id (`RecipeDef.index`) — see
  /// [`crate::packing::pack_recipe`] for the layout.
  pub recipe: u16,
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
  /// `Stack(Up)` the actor and all slot fillers are in `up_length`;
  /// `down_length` is zero. For `Stack(Down)` the reverse. `OnCreate`
  /// always has `up_length = 1, down_length = 0`. Unpack with:
  /// `up = (participants >> 4) & 0x0F`, `down = participants & 0x0F`.
  pub participants: u8,
  /// Bit flags for per-action state that doesn't fit anywhere else
  /// (paused, accelerated, debug-tinted, …). Specific bit assignments
  /// are added as features need them; freshly-started rows start at
  /// `0`. Callers that don't need flags don't have to think about them
  /// — `start_action` zero-initializes the field.
  pub flags: u8,
  /// Scheduled-reducer lag at the time of this row write, in 16-ms
  /// steps (saturating at 255). `0` for client-driven writes;
  /// non-zero only inside a scheduled reducer fire that's running
  /// late. See [`crate::delta_t`].
  pub delta_t: u8,
}

/// Scheduled trigger for `complete_action`. Private — clients have no
/// reason to subscribe; they read `Action.end` instead. Also carries
/// the action's `hex_card_id` (server-only routing detail, kept off
/// the public row).
#[spacetimedb::table(accessor = action_scheduler, scheduled(complete_action))]
#[derive(Debug, Clone)]
pub struct ActionScheduler {
  #[primary_key]
  #[auto_inc]
  pub scheduled_id: u64,
  pub scheduled_at: ScheduleAt,
  #[index(btree)]
  pub action_id: u32,
  /// The hex card the action is anchored to, when the recipe has a
  /// `hex` precondition. `0` means "no hex" (the recipe doesn't
  /// require one, or the resolved hex was a `zones`-only cell with
  /// no `Card` row). Persisted at `start_action` so completion
  /// doesn't have to re-derive the hex from server state — inventory
  /// rows hold `micro_zone = 0` and a chain walk would lose the
  /// relationship, so the matcher's already-resolved id is the
  /// authoritative source. Server-only — not on the public `Action`.
  pub hex_card_id: u32,
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

/// `dead` flag bit on `Action.flags` (mirrors `data/flags.json`'s
/// `actions.dead`). Set as an UPDATE — rather than the row being
/// deleted directly — so the write carries `delta_t` and the
/// client can back-date its end animation by `16 * delta_t` ms.
/// Mirrors `cards.dead` at bit 7 so the client can use one mask
/// across both tables.
pub const FLAG_ACTION_DEAD: u8 = 1 << 7;

/// `canceled` flag bit on `Action.flags` (mirrors
/// `data/flags.json`'s `actions.canceled`). Set together with
/// [`FLAG_ACTION_DEAD`] when the action ended for a reason *other*
/// than normal recipe completion (matcher upgrade, claim drift,
/// reagent stolen by another action, etc.). Lets the client play
/// a different animation for cancellation vs. successful
/// completion. Always set in the same UPDATE as `dead`, never on
/// its own.
pub const FLAG_ACTION_CANCELED: u8 = 1 << 1;

/// How long a dead-flagged action lingers before its actual delete
/// fires. Matches `cards::CARD_REAP_DELAY_SECS` so the client sees
/// dead cards and dead actions vanish on the same cadence.
pub const ACTION_REAP_DELAY_SECS: u32 = 10;

/// Scheduled-deletion queue for actions flagged
/// [`FLAG_ACTION_DEAD`]. Private — clients have no reason to
/// subscribe; the end animation is driven by the `dead` bit flip
/// on the public `Action` row, not by this table. One row per dead
/// action.
///
/// Inserted by [`mark_action_dead`] at the time the action is
/// flagged; `scheduled_at` is `now + ACTION_REAP_DELAY_SECS`.
/// SpacetimeDB fires [`reap_dead_action`] when the time arrives
/// and removes the row from this table after the reducer returns.
#[spacetimedb::table(accessor = pending_action_deletions, scheduled(reap_dead_action))]
#[derive(Debug, Clone)]
pub struct PendingActionDeletion {
  #[primary_key]
  #[auto_inc]
  pub scheduled_id: u64,
  pub scheduled_at: ScheduleAt,
  /// PK of the `Action` row to delete when this fires.
  pub action_id: u32,
}

/// Single chokepoint for action removal. Tears down magnetic state
/// (clears `position_held` flags, deletes the magnetic schedule),
/// releases card holds, deletes the scheduler row, and marks the
/// public `Action` row dead — the actual delete is scheduled via
/// the reaper so the dead-event UPDATE can carry `delta_t` for
/// client latency compensation. Every cancel and complete path
/// goes through here. Calling on a non-magnetic action_id is a
/// no-op for the magnetic half — `magnetic::release` is
/// idempotent.
///
/// `canceled` distinguishes "ended without producing products"
/// (matcher upgrade, claim drift, reagent stolen, etc.) from
/// normal recipe completion. Cancel paths pass `true`; the normal
/// completion path at the end of `complete_action` passes `false`.
/// The flag rides along with `dead` in the same UPDATE so the
/// client can branch its animation on it.
fn delete_action_rows(ctx: &ReducerContext, action_id: u32, canceled: bool) {
  crate::magnetic::release(ctx, action_id);
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
  mark_action_dead(ctx, action_id, canceled);
}

/// Mark an action as dead — sets [`FLAG_ACTION_DEAD`] (and
/// optionally [`FLAG_ACTION_CANCELED`]), stamps `delta_t` (so the
/// client can back-date its end animation), and schedules the
/// actual row deletion for `now + ACTION_REAP_DELAY_SECS`.
///
/// `canceled = true` means the action ended for a reason other
/// than normal recipe completion (matcher upgrade, drift, reagent
/// stolen, etc.). The cancel and dead bits are set in the same
/// UPDATE so the client receives them together.
///
/// Idempotent — calling twice on the same action is a no-op the
/// second time (the dead bit's already set; we don't re-stamp the
/// canceled bit or schedule another reap). Missing action is also
/// a no-op.
///
/// Caller responsibility: the matching `action_scheduler` and
/// `card_holds` rows are private state and should be deleted
/// eagerly *before* this call — once the action is dead the
/// matcher must not see those private rows. `delete_action_rows`
/// does this in the right order.
pub fn mark_action_dead(ctx: &ReducerContext, action_id: u32, canceled: bool) {
  let Some(action) = ctx.db.actions().action_id().find(&action_id) else {
    return;
  };
  if (action.flags & FLAG_ACTION_DEAD) != 0 {
    return;
  }
  let mut updated = action;
  updated.flags |= FLAG_ACTION_DEAD;
  if canceled {
    updated.flags |= FLAG_ACTION_CANCELED;
  }
  updated.delta_t = crate::delta_t::current();
  ctx.db.actions().action_id().update(updated);

  let reap_at = Timestamp::from_micros_since_unix_epoch(
    ctx
      .timestamp
      .to_micros_since_unix_epoch()
      .saturating_add((ACTION_REAP_DELAY_SECS as i64).saturating_mul(1_000_000)),
  );
  ctx.db.pending_action_deletions().insert(PendingActionDeletion {
    scheduled_id: 0,
    scheduled_at: ScheduleAt::Time(reap_at),
    action_id,
  });
}

/// Scheduled reducer — fires `ACTION_REAP_DELAY_SECS` after an
/// action is flagged dead and removes the row. Defended against
/// client-spoofed invocation: the scheduler row must still exist
/// (legitimate fires see it; SpacetimeDB deletes it after this
/// returns). The `Action` row itself may already be gone if some
/// other path removed it directly — `delete` on a missing PK is a
/// silent no-op, which is the right behavior here.
#[reducer]
pub fn reap_dead_action(ctx: &ReducerContext, deletion: PendingActionDeletion) -> Result<(), String> {
  if ctx
    .db
    .pending_action_deletions()
    .scheduled_id()
    .find(&deletion.scheduled_id)
    .is_none()
  {
    return Ok(());
  }
  ctx.db.actions().action_id().delete(&deletion.action_id);
  Ok(())
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
/// stack (caller sums slot weights, plus hex/root tier weights).
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
/// care about specificity. Exposed for `magnetic.rs` (slot-fill candidate
/// matching) — the match shape (Card vs Aspect vs Type vs Any plus the
/// composite combinators) is intentionally a single source of truth.
pub fn entity_matches(entity: &Entity, card_def: &CardDefinition) -> bool {
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
/// Field order **is** the comparison order: `hex_weight` outranks
/// `root_weight` outranks `slot_weight`. Within a tier, the value is the
/// `entity_match_weight` of how that condition was satisfied — so
/// `hex: "forest"` (Card → 4) outranks `hex: ["wood", 1]` (Aspect → 3)
/// in the hex tier without ever consulting root or slots.
///
/// Recipes with no `hex` field score `hex_weight = 0`; same for `root`.
/// `slot_weight` is the sum of per-slot weights, so a recipe with N
/// card-key slots scores 4N — but no number of slot weights can defeat a
/// recipe whose hex/root tier is non-zero, because comparison stops at
/// the first non-equal tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
struct MatchWeight {
  hex_weight: u32,
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
/// where the actor sits — for `Stack(Up)` and `Stack(Down)` recipes
/// alike. Returns `None` if the slot list doesn't fit, any required
/// entity fails to match, or any decode fails.
fn score_recipe_for_actor(
  recipe: &RecipeDef,
  branch_chain: &[Card],
  branch_chain_defs: &[Option<&'static CardDefinition>],
  hex_def: Option<&'static CardDefinition>,
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

  // Hex tier — top of the priority hierarchy. When `recipe.hex` is set,
  // the chain root must be a rectangle attached to a hex (`stacked_state
  // == 3`) AND the hex card's def must satisfy `recipe.hex`. The hex
  // card itself is pre-resolved by the caller and passed in as
  // `hex_def`; if `hex_def` is `None` here while `recipe.hex` is `Some`,
  // the chain isn't on a hex and the recipe doesn't match.
  let hex_weight = if let Some(hex_entity) = &recipe.hex {
    let def = hex_def?;
    let w = entity_match_weight(hex_entity, def);
    if w == 0 {
      return None;
    }
    w
  } else {
    0
  };

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
  // to the pool even though it isn't held). Hex aspects don't
  // contribute today — the hex is a precondition, not a participant.
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
    weight: MatchWeight { hex_weight, root_weight, slot_weight },
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
/// visible chain, scores all `Stack(Up)` recipes, and applies the
/// upgrade rules from the module docs.
///
/// The chain's hex relationship (when the root is rect-on-hex) is
/// read directly from the row state — `submit_inventory_stacks`
/// mirrors the client's submitted `(layer, macro_zone, micro_zone,
/// micro_location)` onto the root's row before reaching here, so
/// `resolve_hex_at_root`'s row-derived path picks it up without an
/// override.
pub fn process_top_branch(
  ctx: &ReducerContext,
  branch_chain_ids: &[u32],
  owner_id: u32,
) -> Result<(), String> {
  process_branch(
    ctx,
    branch_chain_ids,
    RecipeType::Stack(definitions::StackDirection::Up),
    owner_id,
  )
}

/// Same as [`process_top_branch`] for the bottom branch.
/// `branch_chain_ids` is `[root, stack_down[0], stack_down[1], …]`.
pub fn process_bottom_branch(
  ctx: &ReducerContext,
  branch_chain_ids: &[u32],
  owner_id: u32,
) -> Result<(), String> {
  process_branch(
    ctx,
    branch_chain_ids,
    RecipeType::Stack(definitions::StackDirection::Down),
    owner_id,
  )
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

  // The hex (if any) under the chain root is constant across actor
  // positions in this branch — pre-resolve it once so the per-actor
  // scoring loop doesn't re-do the lookup. `None` means the chain
  // root isn't on a hex (or the resolved hex card / zone cell is
  // missing). The resolved `card_id` (when non-zero) is passed all
  // the way down to `start_action` so it can be persisted on
  // `ActionScheduler.hex_card_id` for `complete_action` to re-use.
  let hex_hit = resolve_hex_at_root(ctx, &branch_chain[0], None)?;
  let hex_def = hex_hit.map(|h| h.def);
  let resolved_hex_card_id = hex_hit.map_or(0, |h| h.card_id);

  for actor_idx in 0..branch_chain.len() {
    process_actor_candidate(
      ctx,
      &branch_chain,
      &branch_chain_defs,
      hex_def,
      resolved_hex_card_id,
      actor_idx,
      recipe_type,
      owner_id,
    )?;
  }
  Ok(())
}

/// What both the matcher and the product router need to know about
/// the hex below a chain root. Resolved by [`resolve_hex_at_root`]
/// (or via the actor-side walker [`find_hex_under_actor_chain`]).
#[derive(Debug, Clone, Copy)]
struct HexHit {
  /// The hex card's `card_id` when backed by a `Card` row, otherwise
  /// `0`. Persisted on `ActionScheduler.hex_card_id` (server-only;
  /// not on the public `Action`) so `complete_action` can re-resolve
  /// via the override path even when the actor's chain walk would
  /// lose the relationship (e.g. inventory chains, where rows hold
  /// `micro_zone = 0`).
  card_id: u32,
  /// The hex card's definition. Always present when the hex resolves
  /// — whether from a `Card` row in the `cards` table or by decoding
  /// the corresponding cell from the `zones` table.
  def: &'static CardDefinition,
  /// The hex card's `owner_id` if the hex is backed by a `Card` row.
  /// `0` when the hex was decoded from `zones` only — packed Zone
  /// cells don't carry per-cell ownership, so `(Inventory, Hex)`
  /// product targets fall back to the actor's panel in that case.
  owner_id: u32,
}

/// Resolve the hex below `root`. Three paths, in priority order:
///
/// **1. Client override** (`override_card_id`, used by
/// `submit_inventory_stacks`): inventory cards on the server hold
/// `micro_zone = 0` regardless of the client's local stack state, so
/// the rect-on-hex relationship can't be read from the row. The
/// client carries it on [`crate::cards::InventoryStack::hex`]
/// (extracted from its local root row's `micro_location` when
/// `stacked_state == 3`); when set here, look that card up directly
/// and use its def + `owner_id`. `None` falls through to the
/// row-derived paths.
///
/// **2. `cards` lookup via `root.micro_location`** (rect-on-hex
/// derived from the row's stack_state): when
/// `root.micro_zone & 0b11 == 3` and `root.micro_location != 0`,
/// treat micro_location as a hex card_id. Used by world-layer stacks
/// where stack_state IS tracked server-side, and by magnetic
/// placements that promoted a tile to a real `Card` row.
///
/// **3. `zones` decode** (rect-on-hex over a packed Zone cell):
/// when the cards lookup misses, decode the corresponding cell from
/// the `zones` row at `root.macro_zone`. Owner_id is `0` because
/// packed Zone cells don't carry per-cell ownership.
///
/// Returns `Ok(None)` if no path resolves a hex.
fn resolve_hex_at_root(
  ctx: &ReducerContext,
  root: &Card,
  override_card_id: Option<u32>,
) -> Result<Option<HexHit>, String> {
  // Path 1: client override.
  if let Some(hex_id) = override_card_id {
    if hex_id != 0 {
      if let Some(hex_card) = ctx.db.cards().card_id().find(&hex_id) {
        let Some(def) = card_def_for(&hex_card)? else {
          return Ok(None);
        };
        return Ok(Some(HexHit { card_id: hex_card.card_id, def, owner_id: hex_card.owner_id }));
      }
    }
    // Override supplied but didn't resolve — fall through to the
    // row-derived paths rather than failing outright.
  }
  // Paths 2 & 3 require the chain root's stack_state to indicate
  // rect-on-hex — inventory chains hit `None` here (which is why
  // path 1 exists).
  if (root.micro_zone & 0b11) != 3 {
    return Ok(None);
  }
  // Path 2: cards table via root.micro_location.
  if root.micro_location != 0 {
    if let Some(hex_card) = ctx.db.cards().card_id().find(&root.micro_location) {
      let Some(def) = card_def_for(&hex_card)? else {
        return Ok(None);
      };
      return Ok(Some(HexHit { card_id: hex_card.card_id, def, owner_id: hex_card.owner_id }));
    }
  }
  // Path 3: zones table fallback.
  let Some(zone) = zones::find_zone(ctx, root.layer, root.macro_zone) else {
    return Ok(None);
  };
  let coord = zones::LocalCoord::from_micro_zone(root.micro_zone);
  let cell_id = zones::read_cell(&zone.cell_rows(), coord);
  if cell_id == 0 {
    return Ok(None);
  }
  let packed = zones::cell_packed_definition(zone.packed_definition, cell_id);
  let Some(def) = definitions::decode_definition(packed)? else {
    return Ok(None);
  };
  // Zones-resolved cells aren't backed by a Card row — card_id is
  // `0`. The defense check at completion only inspects `def`, and
  // products with `Hex` owner fall back to the actor's panel when
  // owner_id is 0.
  Ok(Some(HexHit { card_id: 0, def, owner_id: 0 }))
}

/// Apply the upgrade decision for one actor candidate. Single source of
/// truth for the four-way table in the module docs.
fn process_actor_candidate(
  ctx: &ReducerContext,
  branch_chain: &[Card],
  branch_chain_defs: &[Option<&'static CardDefinition>],
  hex_def: Option<&'static CardDefinition>,
  hex_card_id: u32,
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
      hex_def,
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
      delete_action_rows(ctx, a.action_id, /* canceled = */ true);
      Ok(())
    }
    (None, Some((recipe, m))) => {
      start_action(ctx, recipe, actor, &m, hex_card_id, owner_id)?;
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
        delete_action_rows(ctx, a.action_id, /* canceled = */ true);
        start_action(ctx, recipe, actor, &m, hex_card_id, owner_id)?;
        Ok(())
      }
    }
  }
}

/// `OnCreate` matcher. The freshly-created card is both root and
/// actor; the visible chain is the card itself. The recipe identifies
/// the target via either `recipe.hex` (matches a hex-shaped card) or
/// `recipe.root` (matches any card type) — both may be set. The
/// matcher picks the highest-weight recipe whose specified entity (or
/// entities) match the new card's def.
///
/// When the new card matches via `recipe.hex`, its id is also
/// persisted on the started action's `hex_card_id` so completion can
/// re-resolve the hex without walking the chain — relevant for
/// non-magnetic on_create recipes that carry a hex precondition
/// (the id lives on `ActionScheduler`, not `Action`).
/// Magnetic recipes pivot to `magnetic::install` from `start_action`
/// and don't write an `Action` row, so the persistence is only
/// observable for the regular path.
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

  let mut best: Option<(&'static RecipeDef, ActorMatch, u32)> = None;
  for recipe in definitions::recipes_of_type(RecipeType::OnCreate)? {
    // Hex tier — when the recipe specifies `hex`, the new card must
    // be a hex-shaped type AND match the entity. A rect-typed card
    // can't satisfy a hex precondition even if its key/aspects line
    // up.
    let mut hex_weight: u32 = 0;
    let mut hex_card_id_for_action: u32 = 0;
    if let Some(hex_entity) = &recipe.hex {
      if !definitions::is_hex_type(card_def.card_type)? {
        continue;
      }
      let w = entity_match_weight(hex_entity, card_def);
      if w == 0 {
        continue;
      }
      hex_weight = w;
      hex_card_id_for_action = card_id;
    }

    // Root tier — entity match against the new card's def with no
    // shape constraint.
    let mut root_weight: u32 = 0;
    if let Some(root_entity) = &recipe.root {
      let w = entity_match_weight(root_entity, card_def);
      if w == 0 {
        continue;
      }
      root_weight = w;
    }

    // Recipe with neither hex nor root has nothing to match against
    // for `OnCreate`. The parser rejects these, but defense in depth.
    if hex_weight == 0 && root_weight == 0 {
      continue;
    }

    let pool = aspect_pool(&[card_def]);
    let m = ActorMatch {
      weight: MatchWeight { hex_weight, root_weight, slot_weight: 0 },
      claimed: vec![card_id],
      pool,
    };
    match best.as_ref() {
      None => best = Some((recipe, m, hex_card_id_for_action)),
      Some((_, b, _)) if m.weight > b.weight => best = Some((recipe, m, hex_card_id_for_action)),
      _ => {}
    }
  }
  match best {
    Some((recipe, m, hex_card_id)) => {
      let action_id = start_action(ctx, recipe, &card, &m, hex_card_id, card.owner_id)?;
      Ok(Some(action_id))
    }
    None => Ok(None),
  }
}

// ─── Action start / refresh ──────────────────────────────────────────────────

/// Insert an Action row, schedule its completion, and claim every card
/// in the match window. Returns the new `action_id`.
///
/// `hex_card_id` is the hex anchor resolved by the matcher (0 when
/// the recipe has no hex precondition, or the resolved hex was a
/// `zones`-only cell with no `Card` row). Persisted on
/// `ActionScheduler.hex_card_id` (private — kept off the public
/// `Action` row) so `complete_action` doesn't need to re-derive it
/// from server state — necessary for inventory chains where rows
/// hold `micro_zone = 0`.
fn start_action(
  ctx: &ReducerContext,
  recipe: &RecipeDef,
  actor: &Card,
  m: &ActorMatch,
  hex_card_id: u32,
  owner_id: u32,
) -> Result<u32, String> {
  // Magnetic recipes pivot here — instead of going into the actions
  // table, the matched outer recipe installs a magnetic_action that
  // ticks every `recipe.interval` and queues an inner action when an
  // inner's slots fill. See `magnetic.rs`.
  if recipe.magnetic.is_some() {
    return crate::magnetic::install(ctx, recipe, actor, owner_id);
  }

  // Non-magnetic recipes: parser guarantees `duration.is_some()`.
  let duration_def = recipe.duration.as_ref().ok_or_else(|| {
    format!(
      "non-magnetic recipe {:?} reached start_action without a duration — parser invariant broken",
      recipe.id
    )
  })?;
  let duration = resolve_duration(duration_def, &m.pool);
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
  // Stack recipes include the actor as `slots[0]`. OnCreate has the
  // actor implicitly as the new card with no slot list. Magnetic
  // recipes pivoted away above so they don't reach this match.
  let participants = match recipe.recipe_type {
    RecipeType::Stack(definitions::StackDirection::Up) => pack_participants(slot_count, 0),
    RecipeType::Stack(definitions::StackDirection::Down) => pack_participants(0, slot_count),
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
    flags: 0,
    delta_t: crate::delta_t::current(),
  });
  ctx.db.action_scheduler().insert(ActionScheduler {
    scheduled_id: 0,
    scheduled_at: ScheduleAt::Time(complete_at),
    action_id: inserted.action_id,
    hex_card_id,
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

  // Stamp every public-table row write below with the
  // scheduled-reducer lag so the client can back-date animations.
  // `action.end` is the scheduled fire time in unix seconds; the
  // gap between that and `ctx.timestamp` is the lag.
  let _delta_guard = crate::delta_t::enter(crate::delta_t::compute(
    (action.end as i64).saturating_mul(1_000_000),
    ctx.timestamp.to_micros_since_unix_epoch(),
  ));

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
  // for product-target resolution and as the start of the hex walk.
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

  // Resolve the hex (if any) anchored under this action. Prefer the
  // id the matcher persisted on the scheduler at start time —
  // inventory chains hold `micro_zone = 0` server-side, so a chain
  // walk would lose the relationship for stack-on-hex actions. Fall
  // back to walking the actor's chain only when no anchor was
  // recorded (OnCreate paths, and any future world-layer flow that
  // hasn't plumbed `hex_card_id` through `start_action`). `None`
  // means the recipe wasn't on a hex (or the hex has since
  // vanished); the defense check below catches the "vanished" case.
  let hex_hit = if real_scheduler.hex_card_id != 0 {
    resolve_hex_at_root(ctx, &actor, Some(real_scheduler.hex_card_id))?
  } else {
    find_hex_under_actor_chain(ctx, &actor)?
  };

  // Defense-in-depth: re-check that every claimed card's current
  // definition still satisfies the recipe shape, and that the hex
  // precondition (if any) still holds. The upgrade path is supposed
  // to cancel any action whose claim drifted, but a desync (mutated
  // card def, hex moved, lost hold, etc.) shouldn't be able to push
  // a stale completion through. Refuse rather than produce mismatched
  // products.
  if !recipe_still_satisfies_claim(recipe, &claimed_cards, hex_hit.as_ref().map(|h| h.def))? {
    delete_action_rows(ctx, action_id, /* canceled = */ true);
    return Err(format!(
      "complete_action: action {} no longer satisfies recipe {:?} — refused",
      action_id, recipe.id,
    ));
  }

  // For actions queued from the magnetic phase (`flags & MAGNETIC`),
  // the products/reagents come from the *inner* recipe (sub-id'd in
  // the high 4 bits of `flags`), not the outer that lives at
  // `action.recipe`. Resolve here so the rest of completion is
  // source-stable.
  let (products_to_fire, reagents_to_consume, completion_recipe_type) =
    if (action.flags & crate::magnetic::FLAG_ACTION_MAGNETIC) != 0 {
      let bucket = recipe.magnetic.as_ref().ok_or_else(|| {
        format!(
          "complete_action: action {} has MAGNETIC flag but recipe {:?} has no magnetic bucket",
          action_id, recipe.id
        )
      })?;
      let sub_id = crate::magnetic::unpack_action_subid(action.flags) as usize;
      let inner = bucket.inners.get(sub_id).ok_or_else(|| {
        format!(
          "complete_action: sub_id {} out of range for recipe {:?} (have {} inners)",
          sub_id, recipe.id, bucket.inners.len()
        )
      })?;
      (
        inner.products.as_slice(),
        inner.reagents.as_slice(),
        inner.recipe_type,
      )
    } else {
      (
        recipe.products.as_slice(),
        recipe.reagents.as_slice(),
        recipe.recipe_type,
      )
    };

  // Generate products before deleting reagents. The RNG seed is the
  // scheduler id so the outcome is reproducible per-action.
  if !products_to_fire.is_empty() {
    let mut rng_state: u32 = scheduler.scheduled_id as u32;
    generate_products(ctx, products_to_fire, &actor, action.owner_id, hex_hit.as_ref(), &mut rng_state)?;
  }

  // Consume reagents. Each entry is a `Reagent`:
  //
  // - `Reagent::Root` — the chain root. For `OnCreate`, the actor IS
  //   the chain root, so this resolves to `action.card_id`. For stack
  //   recipes the chain root isn't held (multiple recipes can root on
  //   it concurrently) and isn't stored on the `Action` row, so this
  //   is a no-op until chain context-at-completion lands.
  // - `Reagent::Hex` — the hex card the action is anchored to,
  //   recorded on `ActionScheduler.hex_card_id` at start time. `0`
  //   (no anchor) is a no-op.
  // - `Reagent::Slot(1)` — slot 1 is always the actor.
  // - `Reagent::Slot(N)` for `N >= 2` — needs per-slot claim tracking
  //   that doesn't exist yet; no-op for now.
  for &reagent in reagents_to_consume {
    let card_id = match reagent {
      Reagent::Root => match completion_recipe_type {
        RecipeType::OnCreate => action.card_id,
        RecipeType::Stack(_) => continue,
      },
      Reagent::Hex => {
        if real_scheduler.hex_card_id == 0 {
          continue;
        }
        real_scheduler.hex_card_id
      }
      Reagent::Slot(1) => action.card_id,
      Reagent::Slot(_n) => {
        // TODO: slot N >= 2 references positions that aren't
        // recoverable from `card_holds().action_id().filter()`
        // alone (the iteration order is PK, not insertion). Fix
        // by adding a `slot_index: u8` to `CardHold` or by
        // storing the ordered claim list on the `Action` row.
        continue;
      }
    };

    // Cancel any *other* action holding this card before we mark
    // it dead, so we don't strand a CardHold pointing at a row
    // that's about to be reaped. (The current action's hold is
    // fine — it'll be released when `delete_action_rows` runs at
    // the end.)
    if let Some(hold) = ctx.db.card_holds().card_id().find(&card_id) {
      if hold.action_id != action_id {
        delete_action_rows(ctx, hold.action_id, /* canceled = */ true);
      }
    }
    crate::cards::mark_card_dead(ctx, card_id);
  }

  // Tear down. For magnetic actions, this also clears `position_held`
  // off any surviving slot cards (via `magnetic::release`) so the
  // player can drag them again now that the action is over.
  delete_action_rows(ctx, action_id, /* canceled = */ false);
  Ok(())
}

/// Defense-in-depth: verify the claim window and hex precondition are
/// still consistent with the recipe at completion time. The upgrade
/// machinery is supposed to have cancelled any drifted action long
/// before now; this is the belt-and-braces check that runs anyway.
///
/// - **Hex precondition**: when `recipe.hex.is_some()`, `hex_def` must
///   be present and satisfy the hex entity. A hex that drifted to a
///   different definition (or vanished entirely) fails the check.
///   The upgrade machinery already refuses to keep the action running
///   under those conditions, but a stale completion firing during the
///   gap shouldn't be able to produce against a bad hex.
/// - **Claim cards**: every claimed card must still match at least one
///   slot entity. This isn't a strict positional check (`CardHold`
///   doesn't preserve slot index) but it catches a `packed_definition`
///   that's drifted to something the recipe wouldn't accept at any
///   position.
///
/// The chain root isn't held — so it isn't in `claimed_cards` and
/// isn't checked here. The matcher re-validates `recipe.root` against
/// the current chain root on every upgrade pass, which is the only
/// place it can be checked (the chain root isn't recoverable from
/// server state at completion time).
fn recipe_still_satisfies_claim(
  recipe: &RecipeDef,
  claimed_cards: &[Card],
  hex_def: Option<&CardDefinition>,
) -> Result<bool, String> {
  // Hex precondition re-check applies regardless of recipe shape —
  // magnetic, on_create, or stack. If the recipe says "this fires on
  // a despair hex", we want to refuse if the chain isn't on a despair
  // hex anymore, even for magnetic where we trust the tick for the
  // slot/actor invariants.
  if let Some(hex_entity) = &recipe.hex {
    let Some(def) = hex_def else {
      return Ok(false);
    };
    if !entity_matches(hex_entity, def) {
      return Ok(false);
    }
  }

  // Magnetic recipes' `slots` enumerate inputs only — the actor isn't
  // listed there but is still in `claimed_cards`, so a positional
  // slot-match check would always reject the actor and fail. The
  // magnetic tick maintains its own claim invariants every interval;
  // trust them here for the slot side.
  if recipe.magnetic.is_some() {
    return Ok(true);
  }
  for c in claimed_cards {
    let Some(def) = card_def_for(c)? else {
      return Ok(false);
    };
    // OnCreate has empty `slots`; its claim is the actor itself,
    // which the recipe identified via either `root` or `hex` (both
    // may be set). Either matching is sufficient.
    let matches = if recipe.slots.is_empty() {
      let by_root = recipe.root.as_ref().map_or(false, |r| entity_matches(r, def));
      let by_hex = recipe.hex.as_ref().map_or(false, |h| entity_matches(h, def));
      by_root || by_hex
    } else {
      recipe.slots.iter().any(|e| entity_matches(e, def))
    };
    if !matches {
      return Ok(false);
    }
  }
  Ok(true)
}

/// Walk from `start` toward the chain root via `micro_location`,
/// returning the hex below the chain (if any). The walk stops at the
/// first card it finds with `stacked_state == 3` (HEX_ROOT — defer to
/// [`resolve_hex_at_root`] which handles both `Card`-backed and
/// `Zone`-only hexes) or `stacked_state == 0` (LOOSE — chain isn't on
/// a hex). Mid-chain cards (`stacked_state == 1` or `2`) walk through
/// to their parent via `micro_location`.
///
/// `Ok(None)` means the chain isn't on a hex; an actor whose chain
/// dangles (broken `micro_location` link mid-walk) also returns
/// `None`. `Err` surfaces only on a registry-build failure during the
/// hex resolution.
///
/// Used by `complete_action` to route `(Inventory, Hex)` products and
/// re-check the recipe's `hex` precondition. Mid-chain rectangles
/// (`stacked_state` 1 or 2) are still expected to have `Card` rows —
/// only the hex itself can live in `zones` without a `Card`.
fn find_hex_under_actor_chain(
  ctx: &ReducerContext,
  start: &Card,
) -> Result<Option<HexHit>, String> {
  // Bound the walk so a corrupted `micro_location` cycle can't spin
  // forever. Real chains are short (under 16 typically); 64 is
  // generous.
  const MAX_DEPTH: u32 = 64;
  let mut current = start.clone();
  for _ in 0..MAX_DEPTH {
    let stack_state = current.micro_zone & 0b11;
    match stack_state {
      0 => return Ok(None), // loose root
      // No client override at completion time — the magnetic
      // placement or world-layer stack_state on the row is the
      // source of truth here.
      3 => return resolve_hex_at_root(ctx, &current, None),
      _ => {
        // Mid-chain: follow micro_location to parent and continue.
        let Some(parent) = ctx.db.cards().card_id().find(&current.micro_location) else {
          return Ok(None);
        };
        current = parent;
      }
    }
  }
  Ok(None)
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
  products: &[ProductGroup],
  actor: &Card,
  action_owner_id: u32,
  hex: Option<&HexHit>,
  rng: &mut u32,
) -> Result<(), String> {
  for group in products {
    let dest = resolve_product_destination(&group.target, actor, action_owner_id, hex);
    for entity in &group.entities {
      generate_entity_products(ctx, entity, &dest, rng)?;
    }
  }
  Ok(())
}

/// Resolve a product target's `(place, owner)` pair to a concrete
/// destination. Today every place is `Inventory` (`LAYER_INVENTORY`);
/// the owner picks which player's panel.
///
/// Each [`ProductOwner`] variant resolves to a `player_id` via a
/// distinct source — no `macro_zone` ambiguity (which only equals
/// `player_id` on the inventory layer):
///
/// - `Actor` → `actor.owner_id`.
/// - `Action` → `action_owner_id` (= `Action.owner_id`).
/// - `Hex` → `hex.owner_id` (when set and non-zero); otherwise
///   falls back to `action_owner_id`.
/// - `Root` → today, falls back to `action_owner_id` because the
///   chain root isn't held by the action and isn't recoverable from
///   server state at completion. A future change can persist
///   `chain_root_card_id` on `ActionScheduler` (parallel to
///   `hex_card_id`) so this resolves to the chain root's actual
///   `owner_id`.
///
/// `panel_player_id == 0` means "no panel to route to" (world-owned
/// actor, unresolved hex with no fallback) and is handled by
/// [`insert_product`] as a silent skip.
fn resolve_product_destination(
  target: &ProductTarget,
  actor: &Card,
  action_owner_id: u32,
  hex: Option<&HexHit>,
) -> ProductDestination {
  let panel_player_id = match target.owner {
    ProductOwner::Actor => actor.owner_id,
    ProductOwner::Action => action_owner_id,
    ProductOwner::Hex => match hex {
      Some(h) if h.owner_id != 0 => h.owner_id,
      _ => action_owner_id,
    },
    // TODO: persist chain root card_id on `ActionScheduler` (like
    // `hex_card_id`) so we can resolve the actual root owner here
    // instead of falling back to the action initiator. The fallback
    // is correct for the inventory POC where every claimed card is
    // in the same player's panel.
    ProductOwner::Root => action_owner_id,
  };
  match target.place {
    ProductPlace::Inventory => ProductDestination::Panel { panel_player_id },
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
      // World-owned actors (`owner_id == 0`) resolve to panel 0 —
      // there's no player 0, so `insert_card_row` would reject. Skip
      // the product silently rather than aborting the whole
      // completion. Future destinations (loose-on-tile, shared world
      // stash) belong as new `ProductDestination` variants here.
      if *panel_player_id == 0 {
        return Ok(());
      }
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
