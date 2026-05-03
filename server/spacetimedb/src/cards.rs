use spacetimedb::{reducer, ReducerContext, SpacetimeType, Table};
use std::collections::BTreeSet;

use crate::actions;
use crate::players;
// Brings the `players` table-accessor trait into scope so `ctx.db.players()`
// resolves here. `as _` because the trait shares its name with the module
// already imported above.
use crate::players::players as _;

/// Inventory layer. Cards with `layer == LAYER_INVENTORY` live in some
/// player's inventory. World layers will land alongside the world board.
pub const LAYER_INVENTORY: u8 = 1;

#[spacetimedb::table(accessor = cards, public)]
#[derive(Debug, Clone)]
pub struct Card {
  #[primary_key]
  #[auto_inc]
  pub card_id: u32,
  /// Where the card lives. Currently always `LAYER_INVENTORY` (1); world
  /// layers will be added when the world board lands.
  pub layer: u8,
  /// Inventory holder's `player_id`. Clients subscribe on
  /// `macro_zone == own_player_id` to see their inventory. Named `macro_zone`
  /// (rather than `player_id`) because when world cards land this field will
  /// also hold packed `(zone_q:i16, zone_r:i16)` axial coords.
  #[index(btree)]
  pub macro_zone: u32,
  /// World cards: `[local_q:u3][local_r:u3][stack_state:u2]` — in-zone hex
  /// coords plus the card's role in its stack (bits 1..0). The
  /// authoritative stack_state lives here so other cards in the stack can
  /// resolve their parent via `micro_location`. Inventory cards: held at 0;
  /// layout is client-side and the server doesn't track stack_state at
  /// layer 1.
  pub micro_zone: u8,
  /// World cards: variant per `micro_zone`'s `stack_state` — either a
  /// parent `card_id` (for stacked cards) or packed `(i16 x, i16 y)` pixel
  /// coords (for loose cards). Inventory cards: held at 0; layout is
  /// client-side.
  pub micro_location: u32,
  /// Player who owns this card. Not necessarily the player whose inventory
  /// the card sits in (`macro_zone`) — that's how a card can be stashed in
  /// another player's inventory without changing ownership.
  #[index(btree)]
  pub owner_id: u32,
  /// `[card_type:u4][card_category:u4][definition_id:u8]`
  pub packed_definition: u16,
}

// ---------- Card creation ----------

/// Insert a new card row through the shared chokepoint. All card-creation
/// paths should go through this helper so pre-insert validation, hooks, and
/// post-insert side effects (recipe trigger checks, event emission, etc.)
/// only need to land in one place.
///
/// Validates that the layer is supported, that the `owner_id` resolves to
/// an existing `Player`, and that for inventory cards the `macro_zone`
/// (the inventory holder) does too. `card_id` is auto-assigned; callers
/// don't pass one in. `micro_zone` and `micro_location` are zeroed — the
/// inventory layer doesn't track them, and no other layer-specific code
/// path exists yet. When world card creation lands it'll need its own
/// helper (or a layer-aware extension here) that takes those values.
///
/// After insertion, runs the on_create recipe matcher against the new
/// card. Any matching `OnCreate` recipe starts an action immediately.
/// This is also how completion-chains-into-another-recipe works: when a
/// completing action's product passes through here, the new card gets
/// its own on_create check for free.
pub fn insert_card_row(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
  owner_id: u32,
  packed_definition: u16,
) -> Result<Card, String> {
  if ctx.db.players().player_id().find(&owner_id).is_none() {
    return Err(format!(
      "cannot insert card: owner player {} does not exist",
      owner_id,
    ));
  }

  match layer {
    LAYER_INVENTORY => {
      if ctx.db.players().player_id().find(&macro_zone).is_none() {
        return Err(format!(
          "cannot insert inventory card: inventory holder player {} does not exist",
          macro_zone,
        ));
      }
    }
    _ => return Err(format!("unsupported layer {}", layer)),
  }

  let inserted = ctx.db.cards().insert(Card {
    card_id: 0,
    layer,
    macro_zone,
    micro_zone: 0,
    micro_location: 0,
    owner_id,
    packed_definition,
  });
  // Trigger on_create recipe matching for the freshly-created card.
  // Errors propagate (registry-build failures, card lookup failures);
  // a clean "no recipe matched" result returns Ok(None) and we don't
  // care about the action_id.
  actions::try_start_on_create_action(ctx, inserted.card_id)?;
  Ok(inserted)
}

// ---------- Inventory stack submission ----------

/// Maximum cards on either branch of a stack. Total stack size is bounded by
/// `1 + 2 * MAX_STACK_BRANCH`.
pub const MAX_STACK_BRANCH: usize = 16;

/// Maximum number of stacks accepted in a single `submit_inventory_stacks`
/// call. Keeps the per-call work bounded against malicious or buggy clients.
pub const MAX_STACKS_PER_SUBMISSION: usize = 256;

/// One stack the client is asserting as a current arrangement. The card_ids
/// describe composition only; pixel position and stacking layout are
/// client-side state and are not communicated through this struct.
#[derive(SpacetimeType, Debug, Clone)]
pub struct InventoryStack {
  pub root: u32,
  pub stack_up: Vec<u32>,
  pub stack_down: Vec<u32>,
}

/// Client-driven inventory stack submission. The server validates every
/// card_id in every submitted stack belongs to the caller's inventory,
/// then walks each branch through the action upgrade machinery:
///
/// 1. **Top branch.** For each submitted stack, build the up-chain
///    `[root, stack_up[0], …]` and call
///    [`actions::process_top_branch`]. The branch processor iterates
///    every potential actor along the chain and applies the upgrade
///    rules (start, keep, refresh, or cancel) — no blanket pre-cancel.
///    A no-op submission whose stack composition didn't actually
///    change leaves any running action *running*, with its timer
///    untouched.
/// 2. **Bottom branch.** Same shape for `[root, stack_down[0], …]` via
///    [`actions::process_bottom_branch`].
///
/// `OnCreate` triggers fire from [`insert_card_row`], not from here.
#[reducer]
pub fn submit_inventory_stacks(
  ctx: &ReducerContext,
  stacks: Vec<InventoryStack>,
) -> Result<(), String> {
  let player_id = players::resolve_caller(ctx)?;

  if stacks.len() > MAX_STACKS_PER_SUBMISSION {
    return Err(format!(
      "submission has {} stacks (max {})",
      stacks.len(),
      MAX_STACKS_PER_SUBMISSION,
    ));
  }

  let mut seen: BTreeSet<u32> = BTreeSet::new();

  for stack in &stacks {
    if stack.stack_up.len() > MAX_STACK_BRANCH {
      return Err(format!(
        "stack rooted at {} has {} cards on the top branch (max {})",
        stack.root,
        stack.stack_up.len(),
        MAX_STACK_BRANCH,
      ));
    }
    if stack.stack_down.len() > MAX_STACK_BRANCH {
      return Err(format!(
        "stack rooted at {} has {} cards on the bottom branch (max {})",
        stack.root,
        stack.stack_down.len(),
        MAX_STACK_BRANCH,
      ));
    }

    let cards_in_stack = std::iter::once(stack.root)
      .chain(stack.stack_up.iter().copied())
      .chain(stack.stack_down.iter().copied());

    for card_id in cards_in_stack {
      if !seen.insert(card_id) {
        return Err(format!(
          "card {} appears in more than one submitted stack",
          card_id,
        ));
      }

      let card = ctx
        .db
        .cards()
        .card_id()
        .find(&card_id)
        .ok_or_else(|| format!("card {} not found", card_id))?;

      if card.layer != LAYER_INVENTORY {
        return Err(format!(
          "card {} is not on the inventory layer (layer={})",
          card_id, card.layer,
        ));
      }

      if card.macro_zone != player_id {
        return Err(format!(
          "card {} is not in caller's inventory",
          card_id,
        ));
      }
    }
  }

  // ─── Action orchestration ──────────────────────────────────────────
  // Validation passed. Walk each branch through the upgrade machinery.
  // No blanket pre-cancel — `process_*_branch` cancels exactly the
  // actions that are actually disturbed.
  for stack in &stacks {
    // Top branch chain: [root, stack_up[0], stack_up[1], …]
    let mut top_chain: Vec<u32> = Vec::with_capacity(1 + stack.stack_up.len());
    top_chain.push(stack.root);
    top_chain.extend(stack.stack_up.iter().copied());
    actions::process_top_branch(ctx, &top_chain, player_id)?;

    // Bottom branch chain: [root, stack_down[0], stack_down[1], …]
    let mut bottom_chain: Vec<u32> = Vec::with_capacity(1 + stack.stack_down.len());
    bottom_chain.push(stack.root);
    bottom_chain.extend(stack.stack_down.iter().copied());
    actions::process_bottom_branch(ctx, &bottom_chain, player_id)?;
  }

  Ok(())
}
