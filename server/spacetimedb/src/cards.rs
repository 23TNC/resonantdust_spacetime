use spacetimedb::{reducer, ReducerContext, SpacetimeType, Table};
use std::collections::BTreeSet;

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
  /// World cards: in-zone hex coords `[local_q:u3][local_r:u3][reserved:u2]`.
  /// Inventory cards: held at 0; layout is client-side.
  pub micro_zone: u8,
  /// World cards: variant per `stack_state` in `flags` — either a parent
  /// `card_id` (for stacked cards) or packed `(i16 x, i16 y)` pixel coords
  /// (for loose cards). Inventory cards: held at 0; layout is client-side.
  pub micro_location: u32,
  /// Player who owns this card. Not necessarily the player whose inventory
  /// the card sits in (`macro_zone`) — that's how a card can be stashed in
  /// another player's inventory without changing ownership.
  #[index(btree)]
  pub owner_id: u32,
  /// `[card_type:u4][card_category:u4][definition_id:u8]`
  pub packed_definition: u16,
  /// `[stack_state:u2][reserved:u6]` (bits 7..6 stack_state, bits 5..0
  /// reserved). World cards carry the authoritative stack_state so other
  /// cards in the stack can resolve their parent via `micro_location`.
  /// Inventory cards: held at 0; the server does not track stack_state at
  /// layer 1.
  pub flags: u8,
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
/// don't pass one in. `micro_zone`, `micro_location`, and `flags` are
/// zeroed — the inventory layer doesn't track them, and no other
/// layer-specific code path exists yet. When world card creation lands
/// it'll need its own helper (or a layer-aware extension here) that takes
/// those values.
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

  Ok(ctx.db.cards().insert(Card {
    card_id: 0,
    layer,
    macro_zone,
    micro_zone: 0,
    micro_location: 0,
    owner_id,
    packed_definition,
    flags: 0,
  }))
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

/// Client-driven inventory stack submission. The server validates that every
/// card_id in every submitted stack lives in the caller's inventory.
///
/// Future work: cancel actions whose card-set is broken by the new stack
/// composition; trigger new actions for stacks matching a recipe. Both depend
/// on tables that don't exist yet.
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

  Ok(())
}
