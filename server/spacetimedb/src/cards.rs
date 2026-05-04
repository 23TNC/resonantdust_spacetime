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

/// World layer. Cards with `layer == LAYER_WORLD` live on the world board.
/// `macro_zone` stores packed `(zone_q:i16, zone_r:i16)` axial coords for
/// these cards rather than a `player_id`.
pub const LAYER_WORLD: u8 = 2;

/// `local_q = 1` packed into the high 3 bits of `micro_zone` — the
/// "server is asserting this position; trust it" signal. Any non-zero
/// `local_q` satisfies the check; `1` is used specifically so the packed
/// value reads as `0x20 | stack_state` and is easy to spot in a hex dump.
pub const MICRO_ZONE_LOCAL_Q_ONE: u8 = 0x20;

/// Bitmask covering the `local_q` nibble (bits 7..5) of a card's
/// `micro_zone`. The client uses `(micro_zone & MICRO_ZONE_LOCAL_Q_MASK) != 0`
/// as the "server is forcing this position; trust it" signal for
/// inventory cards — without that bit set, the client treats inbound
/// position updates as stale and ignores them (inventory layout is
/// client-owned). Servers that *do* want to force a placement must set
/// some bit in `local_q` so the client picks it up; see
/// [`insert_card_row_at_position`].
pub const MICRO_ZONE_LOCAL_Q_MASK: u8 = 0xE0;

/// `position_hold` flag bit on `Card.flags` (mirrors
/// `data/flags.json`'s `cards.position_hold`). Set by the magnetic
/// system while a card is in flight; cleared on magnetic
/// cancel/completion or when the card lands back in inventory.
pub const FLAG_CARD_POSITION_HOLD: u8 = 1 << 0;

/// `drop_hold` flag bit on `Card.flags` (mirrors
/// `data/flags.json`'s `cards.drop_hold`). Same lifecycle as
/// `position_hold` — held alongside it by magnetic placements.
pub const FLAG_CARD_DROP_HOLD: u8 = 1 << 3;

/// Safety belt: clear `position_hold` + `drop_hold` on `card_id`
/// when it lands on `LAYER_INVENTORY`. Idempotent — safe to call
/// when the card doesn't exist, isn't on inventory, or already has
/// those bits clear. The locked-not-hold variants
/// (`position_locked`, `drop_locked`) stay set per their permanent
/// semantics.
///
/// Call from any path that lands a card on the inventory layer
/// through user input (drag-drop into a panel, server-driven
/// "return to inventory" reducer, admin tool, etc.). Magnetic
/// cancel/completion paths handle their own clears via
/// `magnetic.rs`; this is the catch-all for everything else, so
/// stuck "I can't pick up my card" states can't survive a return
/// to inventory.
pub fn clear_hold_flags_on_inventory_landing(ctx: &ReducerContext, card_id: u32) {
  let Some(card) = ctx.db.cards().card_id().find(&card_id) else {
    return;
  };
  if card.layer != LAYER_INVENTORY {
    return;
  }
  if (card.flags & (FLAG_CARD_POSITION_HOLD | FLAG_CARD_DROP_HOLD)) == 0 {
    return;
  }
  let mut updated = card;
  updated.flags &= !(FLAG_CARD_POSITION_HOLD | FLAG_CARD_DROP_HOLD);
  ctx.db.cards().card_id().update(updated);
}

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
  /// client-side. Indexed so the magnetic chain walk
  /// (`magnetic::find_attached`) can find a card's children in
  /// O(matches + log n) instead of scanning the whole table.
  #[index(btree)]
  pub micro_location: u32,
  /// Player who owns this card. Not necessarily the player whose inventory
  /// the card sits in (`macro_zone`) — that's how a card can be stashed in
  /// another player's inventory without changing ownership.
  #[index(btree)]
  pub owner_id: u32,
  /// `[card_type:u4][card_category:u4][definition_id:u8]`
  pub packed_definition: u16,
  /// Bit flags for per-card state that doesn't fit anywhere else (face-up
  /// vs. face-down, sealed, frozen, debug-tinted, …). Specific bit
  /// assignments are added as features need them; freshly-inserted rows
  /// start at `0`. Callers that don't need flags don't have to think
  /// about them — `insert_card_row` zero-initializes the field.
  pub flags: u8,
}

// ---------- Card creation ----------

/// Insert a new card row through the shared chokepoint. All card-creation
/// paths should go through this helper (or [`insert_card_row_at_position`]
/// for the rare force-position case) so pre-insert validation, hooks, and
/// post-insert side effects (recipe trigger checks, event emission, etc.)
/// only need to land in one place.
///
/// Validates that the layer is supported, that the `owner_id` resolves to
/// an existing `Player`, and that for inventory cards the `macro_zone`
/// (the inventory holder) does too. `card_id` is auto-assigned; callers
/// don't pass one in. `micro_zone` and `micro_location` are zeroed —
/// **inventory invariant**: any card inserted via this helper has
/// `(micro_zone & MICRO_ZONE_LOCAL_Q_MASK) == 0`, which the client reads
/// as "server isn't asserting a position; client owns layout for this
/// card." Use [`insert_card_row_at_position`] when the server actually
/// wants to place a card and have the client trust it.
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
  insert_card_row_inner(ctx, layer, macro_zone, owner_id, packed_definition, 0, 0)
}

/// Insert a card row with caller-supplied `micro_zone` and
/// `micro_location`. Use this only when the server genuinely wants the
/// placement to stick — the typical case is [`insert_card_row`].
///
/// For [`LAYER_INVENTORY`], the client only honors a server-supplied
/// position when `(micro_zone & MICRO_ZONE_LOCAL_Q_MASK) != 0` (some
/// bit in the `local_q` nibble is set); otherwise the inbound update is
/// treated as stale and ignored. We enforce that here so a caller who
/// thinks they're forcing a placement but passes `micro_zone == 0`
/// fails loudly at the server boundary instead of being silently
/// dropped at the client.
///
/// Other layers (world layers, when they land) use `micro_zone` for
/// stack state and don't share the inventory's "trust me" signal —
/// the local-q check above doesn't apply to them.
pub fn insert_card_row_at_position(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
  owner_id: u32,
  packed_definition: u16,
  micro_zone: u8,
  micro_location: u32,
) -> Result<Card, String> {
  if layer == LAYER_INVENTORY && (micro_zone & MICRO_ZONE_LOCAL_Q_MASK) == 0 {
    return Err(format!(
      "insert_card_row_at_position: inventory layer requires `local_q != 0` \
       (micro_zone & 0x{:02X}); a zero local_q is the client's signal to ignore \
       the inbound position. Use insert_card_row if you don't want to force a placement.",
      MICRO_ZONE_LOCAL_Q_MASK,
    ));
  }
  insert_card_row_inner(ctx, layer, macro_zone, owner_id, packed_definition, micro_zone, micro_location)
}

/// Shared insert path for [`insert_card_row`] and
/// [`insert_card_row_at_position`]. Performs layer + player validation,
/// inserts the row, and runs the on_create matcher. Private — public
/// callers go through one of the wrappers so the position contract
/// (zero by default, opt-in non-zero) is enforced at the API surface.
fn insert_card_row_inner(
  ctx: &ReducerContext,
  layer: u8,
  macro_zone: u32,
  owner_id: u32,
  packed_definition: u16,
  micro_zone: u8,
  micro_location: u32,
) -> Result<Card, String> {
  // owner_id == 0 is the "world-owned" sentinel — valid for world cards
  // that belong to no player. Any non-zero owner must resolve to a real player.
  if owner_id != 0 && ctx.db.players().player_id().find(&owner_id).is_none() {
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
    LAYER_WORLD => {
      // macro_zone is packed zone coords, not a player_id — no holder check.
    }
    _ => return Err(format!("unsupported layer {}", layer)),
  }

  let inserted = ctx.db.cards().insert(Card {
    card_id: 0,
    layer,
    macro_zone,
    micro_zone,
    micro_location,
    owner_id,
    packed_definition,
    flags: 0,
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
///
/// `hex` carries the chain-root → hex relationship for inventory stacks
/// placed on a hex card. The client extracts this from its local root
/// row's `micro_location` when its `stacked_state == 3` and forwards it
/// here. The server can't read that relationship from its own row —
/// inventory cards on the server hold `micro_zone = 0` by convention
/// (see `DataManager.applyServerUpdate`'s "server doesn't track
/// inventory positions" branch). `None` for chains not on a hex.
#[derive(SpacetimeType, Debug, Clone)]
pub struct InventoryStack {
  pub root: u32,
  pub stack_up: Vec<u32>,
  pub stack_down: Vec<u32>,
  pub hex: Option<u32>,
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
    actions::process_top_branch(ctx, &top_chain, stack.hex, player_id)?;

    // Bottom branch chain: [root, stack_down[0], stack_down[1], …]
    let mut bottom_chain: Vec<u32> = Vec::with_capacity(1 + stack.stack_down.len());
    bottom_chain.push(stack.root);
    bottom_chain.extend(stack.stack_down.iter().copied());
    actions::process_bottom_branch(ctx, &bottom_chain, stack.hex, player_id)?;
  }

  Ok(())
}
