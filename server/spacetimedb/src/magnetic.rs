//! Magnetic actions — the slot-fill phase that wraps a recipe with a
//! `magnetic` field. The outer recipe matches normally; instead of
//! going into `actions`, it installs a row here. A scheduled tick
//! every `interval` seconds tries to pick up cards from the action
//! owner's inventory that satisfy the recipe's *inner* recipes. When
//! an inner's slot list is fully filled, that inner gets queued into
//! `actions` as a normal action (with `Action.flags` carrying the
//! `MAGNETIC` bit and a 4-bit sub-id pointing back at which inner
//! filled), and the magnetic_action removes itself.
//!
//! Lifecycle:
//!
//! - **Match** an outer magnetic recipe → `install` here, no row in
//!   `actions`.
//! - **Tick** every `recipe.interval` seconds → walk the chain off the
//!   anchor, find any live inner whose next slot is satisfied by a
//!   card in the owner's inventory, place it (`position_hold` +
//!   `drop_hold` + the placement's `local_q = 1`).
//! - **Inner complete** → queue the inner action into `actions`, fire
//!   the **outer** recipe's `products` / `reagents` (its
//!   "complete" event), remove magnetic_action.
//! - **Timeout** → outer's `duration` is the magnetic-phase loop cap
//!   in *ticks*. When `loop_count > cap`, fire outer products,
//!   release `position_hold` + `drop_hold` on every card we placed
//!   (`slot_1..slot_5`), remove magnetic_action.
//! - **Cancel** (e.g. anchor removed) → release flags, remove. No
//!   products fire — cancellation is distinct from completion.
//! - **Inner action completes in `actions`** → because the action's
//!   `flags & MAGNETIC` is set, the completion path walks the chain
//!   from `action.card_id` and clears `position_hold` + `drop_hold`
//!   on every member. Inner products / reagents fire as normal.
//!
//! ## Current scope (POC)
//!
//! This file implements the **despair-recipe path**: hex-anchored
//! outer with a single inner that has only a `root` (no further
//! slots). Multi-slot inners and non-hex anchors are TODO'd inline
//! and will reject at install / tick rather than misbehave.

use spacetimedb::{reducer, ReducerContext, ScheduleAt, Table, TimeDuration, Timestamp};

use std::collections::BTreeMap;

use crate::actions::{
  actions, action_scheduler, card_holds, entity_matches, pack_participants, Action,
  ActionScheduler, CardHold,
};
use crate::cards::{cards, Card, LAYER_INVENTORY, MICRO_ZONE_LOCAL_Q_MASK};
use crate::definitions::{
  self, AspectId, Duration as RecipeDuration, Entity, InnerRecipe, ProductOwner, ProductPlace,
  RecipeDef, RecipeType, StackDirection,
};

// ─── Constants ───────────────────────────────────────────────────────────────

/// Cap on inner recipes per outer (sub-id is 4 bits in `Action.flags`).
pub const MAGNETIC_MAX_SLOTS: usize = 5;

/// Stack-state values written into bits 1..0 of `Card.micro_zone`.
/// Mirrored on the client; if these change, change the client's
/// `cardData.ts` constants too.
pub const STACK_STATE_LOOSE: u8 = 0;
pub const STACK_STATE_TOP: u8 = 1;
pub const STACK_STATE_BOTTOM: u8 = 2;
pub const STACK_STATE_HEX_ROOT: u8 = 3;
const STACK_STATE_MASK: u8 = 0b11;

/// `local_q == 1` packed into the high 3 bits of `micro_zone` — the
/// "trust this position" signal the client uses to accept inbound
/// position updates instead of treating them as stale.
const MICRO_ZONE_LOCAL_Q_ONE: u8 = 0x20;

// Card flag bits — mirrors `data/flags.json`.
const FLAG_POSITION_HOLD: u8 = 1 << 0;
const FLAG_DROP_HOLD: u8 = 1 << 3;

/// Action flag — set on actions queued from the magnetic phase. The
/// completion path checks this to walk the chain and release
/// `position_hold` + `drop_hold`. Mirrors `data/flags.json`'s
/// `actions.magnetic_inputs` (low bit; high 4 bits hold the sub-id).
pub const FLAG_ACTION_MAGNETIC: u8 = 1 << 0;
const FLAG_ACTION_SUBID_SHIFT: u8 = 4;
const FLAG_ACTION_SUBID_MASK: u8 = 0xF0;

#[inline]
pub fn pack_action_flags(magnetic: bool, sub_id: u8) -> u8 {
  let base = if magnetic { FLAG_ACTION_MAGNETIC } else { 0 };
  base | ((sub_id << FLAG_ACTION_SUBID_SHIFT) & FLAG_ACTION_SUBID_MASK)
}

#[inline]
pub fn unpack_action_subid(flags: u8) -> u8 {
  (flags & FLAG_ACTION_SUBID_MASK) >> FLAG_ACTION_SUBID_SHIFT
}

// ─── Tables ─────────────────────────────────────────────────────────────────

/// Public face of a live magnetic action. Clients subscribe to this
/// to render progress UI on the anchor card. Carries only what the
/// renderer needs — the scheduler row [`MagneticActionScheduler`]
/// holds the rest of the live state (slot tally, flags, owner).
///
/// `end` is the unix-seconds timestamp of the **next tick** (the
/// time at which the server will next try to grab a card). Updated
/// every time we persist this row. The client uses
/// `(now, end - interval, end)` to render an "until next pickup"
/// progress bar; the recipe's `duration` (when present) tells the
/// client the loop-count cap so it can also render overall progress
/// from `loop_count`.
///
/// `loop_count` increments each tick that didn't make progress; the
/// terminator (when the recipe has one) is `outer.duration`
/// interpreted as a loop-count cap.
#[spacetimedb::table(accessor = magnetic_actions, public)]
#[derive(Debug, Clone)]
pub struct MagneticAction {
  #[primary_key]
  #[auto_inc]
  pub magnetic_action_id: u32,
  /// The card the magnetic action is "on" — chain root for
  /// hex-anchored magnetic recipes (the hex itself), or the rect
  /// chain root for non-hex magnetic recipes (TBD).
  #[index(btree)]
  pub card_id: u32,
  /// Outer recipe packed stable id (see
  /// [`crate::packing::pack_recipe`]). Inner recipes are sub-id'd
  /// through `Action.flags` once an inner queues into actions.
  pub recipe: u16,
  /// Unix-seconds timestamp of the next scheduled tick (when the
  /// server will next try to grab a card). Refreshed at install and
  /// on every tick that persists this row. The client derives
  /// "time until next pickup" from `(end - now)` and overall
  /// progress from `(loop_count, recipe.duration)`.
  pub end: u32,
  /// Mirrors anchor's layer for subscription scoping (clients
  /// subscribe to `(macro_zone, layer)`).
  pub layer: u8,
  #[index(btree)]
  pub macro_zone: u32,
  /// Tick counter. Increments every no-progress tick. Terminator
  /// compares this against the outer recipe's duration (interpreted
  /// as loop count).
  pub loop_count: u8,
}

/// Private scheduler / state for a magnetic action. Holds the
/// recurring tick schedule, the inventory owner we pull from,
/// per-action flag bits, and the placement tally. Linked to its
/// public [`MagneticAction`] via `magnetic_action_id`. Clients
/// don't subscribe — `slot_1..slot_5` reveal what the server
/// pulled, and `owner_id` is server-routing detail.
#[spacetimedb::table(accessor = magnetic_action_scheduler, scheduled(magnetic_tick))]
#[derive(Debug, Clone)]
pub struct MagneticActionScheduler {
  #[primary_key]
  #[auto_inc]
  pub scheduled_id: u64,
  pub scheduled_at: ScheduleAt,
  /// FK back to the public row.
  #[index(btree)]
  pub magnetic_action_id: u32,
  /// Inventory owner whose cards we pull from.
  pub owner_id: u32,
  /// Reserved for flag bits.
  pub flags: u8,
  /// Cards we've placed onto the chain, in placement order. `0` =
  /// empty slot. Tracked here (rather than re-derived from the
  /// chain) because cancel paths run after the anchor or chain may
  /// already be partially gone, and we still need to know which
  /// cards to clear flags on.
  pub slot_1: u32,
  pub slot_2: u32,
  pub slot_3: u32,
  pub slot_4: u32,
  pub slot_5: u32,
}

// ─── install ────────────────────────────────────────────────────────────────

/// Install a magnetic_action for `recipe`. Called from
/// `actions::start_action` when `recipe.magnetic.is_some()`.
///
/// Inserts both the public [`MagneticAction`] (subscription target
/// for client-side progress UI) and the private
/// [`MagneticActionScheduler`] (recurring tick + slot tally + owner
/// routing). `actor` is the card the magnetic action attaches to —
/// the chain root from the matcher's chain (which for hex-anchored
/// magnetic recipes today is the hex card itself).
///
/// Returns the public `magnetic_action_id` so callers
/// (`start_action`) have a return value shaped like a real
/// `action_id`. The magnetic-action namespace is independent of
/// action_id and the value is purely cosmetic to the caller.
pub fn install(
  ctx: &ReducerContext,
  recipe: &RecipeDef,
  actor: &Card,
  owner_id: u32,
) -> Result<u32, String> {
  let interval_secs = recipe.interval.ok_or_else(|| {
    format!(
      "magnetic recipe {:?} has no `interval` — required by parser, refusing install",
      recipe.id
    )
  })?;

  // `end` is the unix-seconds timestamp of the next tick. The
  // recurring schedule fires `interval_secs` from now and every
  // `interval_secs` after that, so the first next-tick is just
  // `now + interval_secs`. Refreshed inside `magnetic_tick` on
  // every persisted state change.
  let now_secs = ctx_seconds(ctx)?;
  let end = now_secs.saturating_add(interval_secs);

  let public = ctx.db.magnetic_actions().insert(MagneticAction {
    magnetic_action_id: 0,
    card_id: actor.card_id,
    recipe: recipe.index,
    end,
    layer: actor.layer,
    macro_zone: actor.macro_zone,
    loop_count: 0,
  });

  let interval = ScheduleAt::Interval(TimeDuration::from_micros(
    (interval_secs as i64).saturating_mul(1_000_000),
  ));
  ctx.db.magnetic_action_scheduler().insert(MagneticActionScheduler {
    scheduled_id: 0,
    scheduled_at: interval,
    magnetic_action_id: public.magnetic_action_id,
    owner_id,
    flags: 0,
    slot_1: 0,
    slot_2: 0,
    slot_3: 0,
    slot_4: 0,
    slot_5: 0,
  });
  Ok(public.magnetic_action_id)
}

/// Release path called from `actions::delete_action_rows`. In the
/// new model, magnetic_actions and actions are in *separate*
/// namespaces — there's no magnetic_action whose `action_id` matches
/// the action being deleted. So this is a hook for the inner action
/// completion path: when an action with `flags & MAGNETIC` is being
/// torn down, walk its chain and clear `position_hold` + `drop_hold`
/// on every member.
///
/// For magnetic_actions themselves, see [`cancel`] and
/// [`complete_outer`].
pub fn release(ctx: &ReducerContext, action_id: u32) {
  let Some(action) = ctx.db.actions().action_id().find(&action_id) else {
    return;
  };
  if (action.flags & FLAG_ACTION_MAGNETIC) == 0 {
    return;
  }
  // Clear flags on every card the inner action claimed. For
  // slot-based inners the claim list IS the slot list (cards held in
  // place by flags only — no chain attachment), so this is the only
  // place we can recover them. For root-based inners the actor is
  // the placed child of the anchor; the descendant walk below picks
  // up any further-stacked chain members.
  let claimed: Vec<u32> = ctx
    .db
    .card_holds()
    .action_id()
    .filter(&action_id)
    .map(|h| h.card_id)
    .collect();
  for id in claimed {
    clear_hold_flags(ctx, id);
  }
  release_chain_holds(ctx, action.card_id);
}

/// Walk every card stacked on `parent_id` (in any direction) and
/// clear `position_hold` + `drop_hold`. Recurses one level so a
/// 2-card chain (root + slot) gets both cards cleared. Bounded by
/// the magnetic chain length cap.
fn release_chain_holds(ctx: &ReducerContext, parent_id: u32) {
  if parent_id == 0 {
    return;
  }
  // Collect children up front so the inner clear-and-recurse can't
  // race with iteration.
  let children: Vec<Card> = ctx
    .db
    .cards()
    .micro_location()
    .filter(&parent_id)
    .collect();
  for child in children {
    clear_hold_flags(ctx, child.card_id);
    // Recurse into grandchildren — chain may extend further.
    release_chain_holds(ctx, child.card_id);
  }
}

// ─── Tick reducer ───────────────────────────────────────────────────────────

/// Recurring tick — fires per [`MagneticActionScheduler`] row at
/// its `scheduled_at` interval. Walks the anchor's chain to
/// identify which inner recipes are still live, picks up the first
/// matching card from inventory, places it on the chain. Queues an
/// inner action when an inner is fully filled; abandons on timeout.
///
/// Defended against client-spoofed invocation: scheduler row must
/// exist; matching public row must exist; underlying outer recipe
/// must still be in the registry.
#[reducer]
pub fn magnetic_tick(
  ctx: &ReducerContext,
  scheduler: MagneticActionScheduler,
) -> Result<(), String> {
  // Guard 1: the scheduler row must still exist (legitimate fires
  // see it; spoofed/replayed calls don't).
  if ctx
    .db
    .magnetic_action_scheduler()
    .scheduled_id()
    .find(&scheduler.scheduled_id)
    .is_none()
  {
    return Ok(());
  }

  // Look up the public row. Missing means a stale scheduler whose
  // public peer was already deleted — clean up the scheduler too.
  let Some(public) = ctx
    .db
    .magnetic_actions()
    .magnetic_action_id()
    .find(&scheduler.magnetic_action_id)
  else {
    ctx.db.magnetic_action_scheduler().scheduled_id().delete(&scheduler.scheduled_id);
    return Ok(());
  };

  // Guard 2: anchor must still exist. If gone, cancel.
  let Some(anchor) = ctx.db.cards().card_id().find(&public.card_id) else {
    cancel(ctx, scheduler.scheduled_id);
    return Ok(());
  };

  // Guard 3: outer recipe must still be in the registry, must still
  // carry `magnetic`. If not, defensive cancel.
  let Some(outer) = definitions::recipe(public.recipe)? else {
    cancel(ctx, scheduler.scheduled_id);
    return Ok(());
  };
  let Some(bucket) = outer.magnetic.as_ref() else {
    cancel(ctx, scheduler.scheduled_id);
    return Ok(());
  };

  // Anchor's def — used to decide hex-vs-rect placement.
  let anchor_is_hex = match definitions::decode_definition(anchor.packed_definition)? {
    Some(def) => definitions::is_hex_type(def.card_type)?,
    None => false,
  };

  // The next tick will fire `interval` seconds from now. Persist
  // that timestamp on any public-row update so the client always
  // sees a fresh "until next pickup" target.
  let interval_secs = outer.interval.ok_or_else(|| {
    format!(
      "magnetic_tick: recipe {:?} lost its `interval` between install and tick",
      outer.id
    )
  })?;
  let next_tick_end = ctx_seconds(ctx)?.saturating_add(interval_secs);

  // Try to find an inner that can take a pickup this tick. Iterate
  // inners; first one with a matching candidate in inventory wins
  // (the user's "search greedily" rule).
  for (sub_id, inner) in bucket.inners.iter().enumerate() {
    if !inner_is_compatible_with_anchor(inner, anchor_is_hex) {
      continue;
    }

    // Branch on inner shape — root-based places one card as a
    // chain child of the anchor; slot-based pulls N cards from
    // inventory into scheduler.slot_1..N (held in place via flags only).
    if let Some(root_entity) = inner.root.as_ref() {
      // ── Root-based ──
      if let Some(_existing) = find_filling_child(ctx, &anchor, root_entity, inner_first_state(inner, anchor_is_hex))? {
        return queue_and_complete_outer(ctx, &public, &scheduler, &outer, inner, sub_id as u8, anchor_is_hex);
      }
      if let Some(candidate) = find_inventory_candidate(ctx, scheduler.owner_id, root_entity)? {
        let placed_state = inner_first_state(inner, anchor_is_hex);
        let placed_card_id = place_card(
          ctx,
          candidate.card_id,
          anchor.card_id,
          anchor.layer,
          placed_state,
        )?;
        record_placement(ctx, &scheduler, placed_card_id, /* slot_index = */ 0);
        return queue_and_complete_outer(ctx, &public, &scheduler, &outer, inner, sub_id as u8, anchor_is_hex);
      }
    } else {
      // ── Slot-based ──
      let slot_ids = [scheduler.slot_1, scheduler.slot_2, scheduler.slot_3, scheduler.slot_4, scheduler.slot_5];
      let filled = slot_ids.iter().take_while(|&&id| id != 0).count();
      let needed = inner.slots.len();

      if filled >= needed {
        return queue_and_complete_outer(ctx, &public, &scheduler, &outer, inner, sub_id as u8, anchor_is_hex);
      }

      let next_entity = &inner.slots[filled];
      if let Some(candidate) = find_inventory_candidate(ctx, scheduler.owner_id, next_entity)? {
        hold_for_slot(ctx, candidate.card_id)?;
        record_placement(ctx, &scheduler, candidate.card_id, filled);
        // Mirror the write into a local copy so
        // `find_inner_actor_card_id` (which reads `slot_1` for the
        // queue path) sees the fresh state — the parameter
        // `scheduler` is the stale snapshot the runtime passed in.
        let mut updated_scheduler = scheduler.clone();
        match filled {
          0 => updated_scheduler.slot_1 = candidate.card_id,
          1 => updated_scheduler.slot_2 = candidate.card_id,
          2 => updated_scheduler.slot_3 = candidate.card_id,
          3 => updated_scheduler.slot_4 = candidate.card_id,
          4 => updated_scheduler.slot_5 = candidate.card_id,
          _ => return Err(format!("magnetic_tick: slot index {} exceeds magnetic chain cap", filled)),
        }
        if filled + 1 >= needed {
          return queue_and_complete_outer(ctx, &public, &updated_scheduler, &outer, inner, sub_id as u8, anchor_is_hex);
        }
        // Made progress this tick — don't burn timeout budget, but
        // do refresh `end` so the client's "next pickup" countdown
        // restarts.
        let mut updated_public = public.clone();
        updated_public.end = next_tick_end;
        ctx.db.magnetic_actions().magnetic_action_id().update(updated_public);
        return Ok(());
      }
    }
  }

  // No inner filled this tick. Increment loop count and refresh
  // `end` on the public row.
  let mut updated_public = public.clone();
  updated_public.loop_count = updated_public.loop_count.saturating_add(1);
  updated_public.end = next_tick_end;

  // Timeout? Outer.duration as loop-count cap. None means no
  // terminator — magnetic action runs until cancel.
  if let Some(cap) = outer_loop_cap(&outer) {
    if updated_public.loop_count > cap {
      return complete_outer(ctx, &updated_public, &scheduler, &outer, /* fire_products = */ true, /* clear_flags = */ true);
    }
  }

  // Persist updated loop_count + end to the public row.
  ctx.db.magnetic_actions().magnetic_action_id().update(updated_public);
  Ok(())
}

// ─── inner-eval helpers ──────────────────────────────────────────────────────

/// Whether `inner` can be filled given the anchor's shape. Two
/// supported shapes today, both hex-anchored and Stack-typed:
///
/// - **Root-based** (`inner.root.is_some()`): one card placed on the
///   anchor with `stacked_state == 3` is the inner's actor.
/// - **Slot-based** (`!inner.slots.is_empty()`, no `inner.root`): N
///   cards pulled from the owner's inventory into `slot_1..N`,
///   marked with `position_hold` + `drop_hold` but otherwise left in
///   inventory. Slot 1 becomes the inner's actor.
///
/// Rect-anchored inners and inners that mix `root` + `slots` are TBD.
fn inner_is_compatible_with_anchor(inner: &InnerRecipe, anchor_is_hex: bool) -> bool {
  match (inner.recipe_type, anchor_is_hex) {
    (RecipeType::Stack(_), true) => inner.root.is_some() || !inner.slots.is_empty(),
    _ => false,
  }
}

/// What `stacked_state` should the *first* card of an inner take?
/// For a hex anchor, the first card is a rect-on-hex root (state 3).
/// For a rect anchor, it's stacked top/bottom per the inner's
/// direction. Slots 1+ always follow the inner's direction.
fn inner_first_state(inner: &InnerRecipe, anchor_is_hex: bool) -> u8 {
  if anchor_is_hex {
    STACK_STATE_HEX_ROOT
  } else {
    match inner.recipe_type {
      RecipeType::Stack(StackDirection::Up) => STACK_STATE_TOP,
      RecipeType::Stack(StackDirection::Down) => STACK_STATE_BOTTOM,
      RecipeType::OnCreate => STACK_STATE_LOOSE, // shouldn't happen — gated above
    }
  }
}

/// Find a child of `parent` with `target_state` whose definition
/// satisfies `entity`. Returns the matching card if any.
fn find_filling_child(
  ctx: &ReducerContext,
  parent: &Card,
  entity: &Entity,
  target_state: u8,
) -> Result<Option<Card>, String> {
  for child in ctx.db.cards().micro_location().filter(&parent.card_id) {
    if (child.micro_zone & STACK_STATE_MASK) != target_state {
      continue;
    }
    let Some(def) = definitions::decode_definition(child.packed_definition)? else {
      continue;
    };
    if entity_matches(entity, def) {
      return Ok(Some(child));
    }
  }
  Ok(None)
}

/// Walk the action owner's inventory for the first card matching
/// `entity`. Skips cards held by another action's `CardHold`, the
/// actor itself shouldn't be in inventory, and cards already with
/// `position_hold` set.
fn find_inventory_candidate(
  ctx: &ReducerContext,
  owner_id: u32,
  entity: &Entity,
) -> Result<Option<Card>, String> {
  for card in ctx.db.cards().owner_id().filter(&owner_id) {
    if card.layer != LAYER_INVENTORY {
      continue;
    }
    if (card.flags & FLAG_POSITION_HOLD) != 0 {
      continue;
    }
    if ctx.db.card_holds().card_id().find(&card.card_id).is_some() {
      continue;
    }
    let Some(def) = definitions::decode_definition(card.packed_definition)? else {
      continue;
    };
    if entity_matches(entity, def) {
      return Ok(Some(card));
    }
  }
  Ok(None)
}

// ─── placement ──────────────────────────────────────────────────────────────

/// Force `card_id` to be stacked on `parent_id` with the given
/// `stack_state`. Sets `local_q = 1` so the client trusts the
/// inbound position update, promotes the card to `dest_layer` (so
/// inventory cards end up on the world layer alongside their
/// anchor), and stamps `position_hold` + `drop_hold`. Returns the
/// placed card_id for the caller to record.
fn place_card(
  ctx: &ReducerContext,
  card_id: u32,
  parent_id: u32,
  dest_layer: u8,
  stack_state: u8,
) -> Result<u32, String> {
  let Some(card) = ctx.db.cards().card_id().find(&card_id) else {
    return Err(format!("place_card: card {} missing", card_id));
  };
  let new_micro_zone = MICRO_ZONE_LOCAL_Q_ONE | (stack_state & STACK_STATE_MASK);
  debug_assert!(
    (new_micro_zone & MICRO_ZONE_LOCAL_Q_MASK) != 0,
    "magnetic placement micro_zone must set local_q"
  );
  let mut updated = card;
  updated.layer = dest_layer;
  updated.micro_zone = new_micro_zone;
  updated.micro_location = parent_id;
  updated.flags |= FLAG_POSITION_HOLD | FLAG_DROP_HOLD;
  ctx.db.cards().card_id().update(updated);
  Ok(card_id)
}

/// Mark `card_id` as magnetically held in place — sets
/// `position_hold` + `drop_hold` without changing the card's parent,
/// layer, or position. Used by slot-based inners where the pulled
/// card should stay visually in inventory but become un-draggable.
/// Idempotent — already-set flags are left alone.
fn hold_for_slot(ctx: &ReducerContext, card_id: u32) -> Result<(), String> {
  let Some(card) = ctx.db.cards().card_id().find(&card_id) else {
    return Err(format!("hold_for_slot: card {} missing", card_id));
  };
  let mut updated = card;
  updated.flags |= FLAG_POSITION_HOLD | FLAG_DROP_HOLD;
  ctx.db.cards().card_id().update(updated);
  Ok(())
}

/// Record `card_id` into `scheduler.slot_{slot_index+1}` and write
/// the scheduler row back. Used when a magnetic_action is in flight
/// (we keep a tally of what we placed so cancel/timeout can clear
/// flags).
fn record_placement(
  ctx: &ReducerContext,
  scheduler: &MagneticActionScheduler,
  card_id: u32,
  slot_index: usize,
) {
  let mut updated = scheduler.clone();
  match slot_index {
    0 => updated.slot_1 = card_id,
    1 => updated.slot_2 = card_id,
    2 => updated.slot_3 = card_id,
    3 => updated.slot_4 = card_id,
    4 => updated.slot_5 = card_id,
    _ => return, // Out of range — silently drop; magnetic chain cap.
  }
  ctx.db.magnetic_action_scheduler().scheduled_id().update(updated);
}

// ─── completion / cancellation ──────────────────────────────────────────────

/// Outer-completion sequence used both by "queue" (success) and
/// "timeout" branches. Fires outer products if `fire_products`,
/// clears placed-card flags if `clear_flags`, and deletes both the
/// public and scheduler rows.
fn complete_outer(
  ctx: &ReducerContext,
  public: &MagneticAction,
  scheduler: &MagneticActionScheduler,
  outer: &RecipeDef,
  fire_products: bool,
  clear_flags: bool,
) -> Result<(), String> {
  if fire_products && !outer.products.is_empty() {
    // For magnetic outer products, the "actor" referent is the anchor
    // (the card the magnetic action is on). Hex resolution walks from
    // the anchor — for hex-anchored magnetic, the anchor IS the hex
    // card, so the walk's check (`stacked_state == 3`) won't match
    // (the anchor itself isn't rect-on-hex). To still route
    // `Owner::Hex` products to a hex-owner panel, treat the anchor
    // card itself as the hex when it is hex-shaped.
    let Some(anchor) = ctx.db.cards().card_id().find(&public.card_id) else {
      // Anchor gone — skip products. Cancellation path handles
      // remaining cleanup.
      return Ok(());
    };
    let mut rng_state: u32 = scheduler.scheduled_id as u32;
    generate_outer_products(ctx, outer, &anchor, scheduler.owner_id, &mut rng_state)?;
  }
  if clear_flags {
    for &slot in &[scheduler.slot_1, scheduler.slot_2, scheduler.slot_3, scheduler.slot_4, scheduler.slot_5] {
      if slot != 0 {
        clear_hold_flags(ctx, slot);
      }
    }
  }
  ctx.db.magnetic_actions().magnetic_action_id().delete(&public.magnetic_action_id);
  ctx.db.magnetic_action_scheduler().scheduled_id().delete(&scheduler.scheduled_id);
  Ok(())
}

/// Cancel a magnetic_action by its scheduler `scheduled_id`. Looks
/// up the scheduler + matching public row, clears `position_hold` /
/// `drop_hold` on every recorded slot card, and deletes both rows.
/// Does NOT fire outer products (cancellation is distinct from
/// completion). Idempotent — missing scheduler / public is a no-op.
pub fn cancel(ctx: &ReducerContext, scheduled_id: u64) {
  let Some(scheduler) = ctx.db.magnetic_action_scheduler().scheduled_id().find(&scheduled_id) else {
    return;
  };
  for &slot in &[scheduler.slot_1, scheduler.slot_2, scheduler.slot_3, scheduler.slot_4, scheduler.slot_5] {
    if slot != 0 {
      clear_hold_flags(ctx, slot);
    }
  }
  ctx.db.magnetic_actions().magnetic_action_id().delete(&scheduler.magnetic_action_id);
  ctx.db.magnetic_action_scheduler().scheduled_id().delete(&scheduled_id);
}

/// Glue between "an inner is filled" and "complete the outer's
/// magnetic phase". Queues the inner action and fires the outer's
/// products (the magnetic_action's "completion event"); held flags
/// stay set because the inner action now owns the chain.
fn queue_and_complete_outer(
  ctx: &ReducerContext,
  public: &MagneticAction,
  scheduler: &MagneticActionScheduler,
  outer: &RecipeDef,
  inner: &InnerRecipe,
  sub_id: u8,
  anchor_is_hex: bool,
) -> Result<(), String> {
  // The actor of the queued inner action is the first chain member
  // — for hex-anchored single-root inners, that's the rect we just
  // placed on the hex.
  let actor_card_id = find_inner_actor_card_id(ctx, public, scheduler, inner, anchor_is_hex)?
    .ok_or_else(|| format!(
      "queue_and_complete_outer: couldn't find inner actor for magnetic_action {}",
      public.magnetic_action_id
    ))?;
  // Hex anchor → the public row's `card_id` IS the hex card.
  // Persisting it on the queued inner so `complete_action` can
  // resolve the hex precondition without walking the chain (the
  // queued actor's chain on inventory rows holds `micro_zone = 0`).
  let inner_hex_card_id = if anchor_is_hex { public.card_id } else { 0 };
  queue_inner_action(ctx, scheduler, outer, inner, sub_id, actor_card_id, inner_hex_card_id)?;
  // Don't clear flags — inner action now owns the chain.
  complete_outer(ctx, public, scheduler, outer, /* fire_products = */ true, /* clear_flags = */ false)
}

/// For a freshly-completed inner, find the chain card that becomes
/// the inner action's actor. Mirrors the dispatch order in
/// `magnetic_tick` — `root` takes precedence over `slots` when both
/// are present so the actor identification stays consistent.
///
/// - **Root-based inner**: the rect we placed on the hex (state 3) —
///   recover it by scanning the anchor's children.
/// - **Slot-based inner**: `scheduler.slot_1` (slot 1 is always the
///   actor, per the recipe convention).
fn find_inner_actor_card_id(
  ctx: &ReducerContext,
  public: &MagneticAction,
  scheduler: &MagneticActionScheduler,
  inner: &InnerRecipe,
  anchor_is_hex: bool,
) -> Result<Option<u32>, String> {
  if !anchor_is_hex {
    return Ok(None);
  }
  if let Some(root_entity) = inner.root.as_ref() {
    let Some(anchor) = ctx.db.cards().card_id().find(&public.card_id) else {
      return Ok(None);
    };
    return Ok(
      find_filling_child(ctx, &anchor, root_entity, STACK_STATE_HEX_ROOT)?
        .map(|c| c.card_id),
    );
  }
  if !inner.slots.is_empty() {
    return Ok(if scheduler.slot_1 != 0 { Some(scheduler.slot_1) } else { None });
  }
  Ok(None)
}

/// Insert the inner action into `actions` with the magnetic flag and
/// sub-id. Mirrors the body of `actions::start_action` for the
/// non-magnetic branch but takes its inputs from the inner recipe
/// instead of the outer chain match.
fn queue_inner_action(
  ctx: &ReducerContext,
  scheduler: &MagneticActionScheduler,
  outer: &RecipeDef,
  inner: &InnerRecipe,
  sub_id: u8,
  actor_card_id: u32,
  hex_card_id: u32,
) -> Result<(), String> {
  let Some(actor) = ctx.db.cards().card_id().find(&actor_card_id) else {
    return Err(format!(
      "queue_inner_action: actor card {} not found",
      actor_card_id
    ));
  };
  // Aspect pool — for slot-based inners, sum aspects across every
  // pulled slot card; for root-based, just the actor (the single
  // chain member). Drives `resolve_duration_secs` for conditional
  // inner durations.
  let claim_ids: Vec<u32> = if !inner.slots.is_empty() && inner.root.is_none() {
    [scheduler.slot_1, scheduler.slot_2, scheduler.slot_3, scheduler.slot_4, scheduler.slot_5]
      .into_iter()
      .filter(|&id| id != 0)
      .collect()
  } else {
    vec![actor.card_id]
  };
  let mut pool: BTreeMap<AspectId, i32> = BTreeMap::new();
  for &cid in &claim_ids {
    if let Some(c) = ctx.db.cards().card_id().find(&cid) {
      if let Some(def) = definitions::decode_definition(c.packed_definition)? {
        for (aid, val) in &def.aspects {
          *pool.entry(*aid).or_insert(0) += val;
        }
      }
    }
  }
  let duration_secs = resolve_duration_secs(&inner.duration, &pool);
  let now_secs = ctx_seconds(ctx)?;
  let end = now_secs.saturating_add(duration_secs);
  let complete_at = Timestamp::from_micros_since_unix_epoch(
    ctx
      .timestamp
      .to_micros_since_unix_epoch()
      .saturating_add((duration_secs as i64).saturating_mul(1_000_000)),
  );

  // Participants packing: chain length is 1 (root-based, single-root)
  // or `inner.slots.len()` (slot-based). Direction follows the inner's
  // recipe_type.
  let chain_len = if !inner.slots.is_empty() && inner.root.is_none() {
    inner.slots.len() as u8
  } else {
    1
  };
  let participants = match inner.recipe_type {
    RecipeType::Stack(StackDirection::Up) => pack_participants(chain_len, 0),
    RecipeType::Stack(StackDirection::Down) => pack_participants(0, chain_len),
    RecipeType::OnCreate => pack_participants(1, 0),
  };

  let inserted = ctx.db.actions().insert(Action {
    action_id: 0,
    card_id: actor.card_id,
    recipe: outer.index, // outer id; sub_id in flags resolves the inner
    owner_id: scheduler.owner_id,
    layer: actor.layer,
    macro_zone: actor.macro_zone,
    end,
    participants,
    flags: pack_action_flags(true, sub_id),
  });
  ctx.db.action_scheduler().insert(ActionScheduler {
    scheduled_id: 0,
    scheduled_at: ScheduleAt::Time(complete_at),
    action_id: inserted.action_id,
    hex_card_id,
  });
  // Claim every slot/actor card for the inner action so other systems
  // see them as held. Slot-based inners claim the full slot list;
  // root-based inners claim just the actor.
  for &cid in &claim_ids {
    ctx.db.card_holds().insert(CardHold {
      card_id: cid,
      action_id: inserted.action_id,
    });
  }
  Ok(())
}

// ─── inline helpers (kept off the actions.rs API surface) ───────────────────

/// Resolve a recipe `Duration` against an aspect pool to seconds. A
/// trimmed copy of `actions.rs::resolve_duration` — kept inline so
/// magnetic.rs doesn't need to expose private helpers.
fn resolve_duration_secs(d: &RecipeDuration, pool: &BTreeMap<AspectId, i32>) -> u32 {
  match d {
    RecipeDuration::Fixed(s) => *s,
    RecipeDuration::Conditional { cases, fallback } => {
      for (s, cond) in cases {
        if pool_satisfies_inline(cond, pool) {
          return *s;
        }
      }
      *fallback
    }
  }
}

/// Aspect-pool satisfaction check — Same shape as the equivalent
/// helper in actions.rs but inlined here.
fn pool_satisfies_inline(entity: &Entity, pool: &BTreeMap<AspectId, i32>) -> bool {
  match entity {
    Entity::Card(_) | Entity::Type(_) => false,
    Entity::Any => true,
    Entity::Aspect(aspect_id, min) => pool.get(aspect_id).map_or(false, |v| v >= min),
    Entity::And(a, b) => pool_satisfies_inline(a, pool) && pool_satisfies_inline(b, pool),
    Entity::Or(a, b) | Entity::WeightedOr { a, b, .. } => {
      pool_satisfies_inline(a, pool) || pool_satisfies_inline(b, pool)
    }
  }
}

/// `ctx.timestamp` → unix seconds u32.
fn ctx_seconds(ctx: &ReducerContext) -> Result<u32, String> {
  let micros = ctx.timestamp.to_micros_since_unix_epoch();
  if micros < 0 {
    return Err("ReducerContext timestamp is before Unix epoch".to_string());
  }
  let secs = (micros / 1_000_000) as u64;
  u32::try_from(secs).map_err(|_| "ReducerContext timestamp exceeds u32 seconds range".to_string())
}

/// Generate outer products for a magnetic_action's "complete" event
/// (queue or timeout). Mirrors the [`ProductOwner`] semantics from
/// `actions::resolve_product_destination`, but the anchor stands in
/// for the actor (a magnetic outer doesn't have a queued
/// `Action.card_id`):
///
/// - `Actor` → `anchor.owner_id`. The anchor is the magnetic
///   action's "card."
/// - `Action` → `action_owner_id` (= `MagneticActionScheduler.owner_id`,
///   the inventory owner we pull from).
/// - `Hex` → for hex-anchored magnetic recipes the anchor IS the
///   hex, so this resolves to `anchor.owner_id`. Falls back to
///   `action_owner_id` for non-hex anchors or unowned anchors.
/// - `Root` → falls back to `action_owner_id` (chain root isn't
///   tracked by the magnetic_action_scheduler today).
///
/// Groups whose resolved panel is `0` are skipped silently.
fn generate_outer_products(
  ctx: &ReducerContext,
  recipe: &RecipeDef,
  anchor: &Card,
  action_owner_id: u32,
  rng: &mut u32,
) -> Result<(), String> {
  for group in &recipe.products {
    if !matches!(group.target.place, ProductPlace::Inventory) {
      // Only inventory destinations supported today.
      continue;
    }
    let panel_player_id = match group.target.owner {
      ProductOwner::Actor => anchor.owner_id,
      ProductOwner::Action => action_owner_id,
      ProductOwner::Hex => {
        if anchor.owner_id != 0 {
          anchor.owner_id
        } else {
          action_owner_id
        }
      }
      ProductOwner::Root => action_owner_id,
    };
    if panel_player_id == 0 {
      continue;
    }
    for entity in &group.entities {
      generate_one_product(ctx, entity, panel_player_id, rng)?;
    }
  }
  Ok(())
}

/// Insert one product card by entity. Mirrors
/// `actions.rs::generate_entity_products` for the simple cases used
/// by magnetic outer products. Kept inline to avoid widening the
/// actions.rs API surface.
fn generate_one_product(
  ctx: &ReducerContext,
  entity: &Entity,
  panel_player_id: u32,
  rng: &mut u32,
) -> Result<(), String> {
  match entity {
    Entity::Card(name) => {
      if let Some(packed) = definitions::find_packed_by_key(name)? {
        crate::cards::insert_card_row(
          ctx,
          LAYER_INVENTORY,
          panel_player_id,
          panel_player_id,
          packed,
        )?;
      }
    }
    Entity::And(a, b) => {
      generate_one_product(ctx, a, panel_player_id, rng)?;
      generate_one_product(ctx, b, panel_player_id, rng)?;
    }
    Entity::Or(a, _) => {
      generate_one_product(ctx, a, panel_player_id, rng)?;
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
        generate_one_product(ctx, a, panel_player_id, rng)?;
      } else {
        generate_one_product(ctx, b, panel_player_id, rng)?;
      }
    }
    Entity::Aspect(_, _) | Entity::Type(_) | Entity::Any => {
      // Slot-side constructs that don't describe a card to produce.
    }
  }
  Ok(())
}

/// Resolve the outer recipe's `duration` field into a loop-count
/// cap. Conditional durations don't quite fit the magnetic
/// loop-count semantics (no aspect pool to match against at install
/// time); for now, only `Fixed` durations terminate, conditional
/// outer durations are treated as "no terminator".
fn outer_loop_cap(outer: &RecipeDef) -> Option<u8> {
  match outer.duration.as_ref()? {
    RecipeDuration::Fixed(n) => Some((*n).min(255) as u8),
    RecipeDuration::Conditional { .. } => None,
  }
}

/// Clear `position_hold` + `drop_hold` flags on a card. Used by
/// cancel and timeout paths and by `release` for inner action
/// completion. Position/drop-locked variants stay set (those are
/// permanent locks).
fn clear_hold_flags(ctx: &ReducerContext, card_id: u32) {
  let Some(card) = ctx.db.cards().card_id().find(&card_id) else {
    return;
  };
  if (card.flags & (FLAG_POSITION_HOLD | FLAG_DROP_HOLD)) == 0 {
    return;
  }
  let mut updated = card;
  updated.flags &= !(FLAG_POSITION_HOLD | FLAG_DROP_HOLD);
  ctx.db.cards().card_id().update(updated);
}
