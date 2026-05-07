use spacetimedb::{reducer, ReducerContext, ScheduleAt, SpacetimeType, Table, Timestamp};
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
pub const LAYER_WORLD: u8 = 64;

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

/// `dead` flag bit on `Card.flags` (mirrors `data/flags.json`'s
/// `cards.dead`). Set as an UPDATE — rather than the row being
/// deleted directly — so the write carries `delta_t` and the
/// client can back-date its dying animation by `32 * delta_t` ms.
/// Cards-only: actions and magnetic_actions delete immediately
/// and have no equivalent bit.
pub const FLAG_CARD_DEAD: u8 = 1 << 7;

/// How long a dead-flagged card lingers before its actual delete
/// fires. Long enough for the client's death animation to play out
/// (~0.5–2 s) plus headroom for late subscribers; short enough that
/// the row doesn't keep client-side trackers stuck on it.
pub const CARD_REAP_DELAY_SECS: u32 = 10;

/// Clear `position_hold` + `drop_hold` on `card_id`. Called by
/// `actions::release_holds_for_action` on every claimed card at action
/// completion — magnetic or not — so the player can drag those cards
/// again once the action ends. The locked variants (`position_locked`,
/// `drop_locked`) are never touched here. Idempotent — no-op if the
/// card doesn't exist or the bits are already clear.
pub fn clear_action_hold_flags(ctx: &ReducerContext, card_id: u32) {
  let Some(card) = ctx.db.cards().card_id().find(&card_id) else {
    return;
  };
  if (card.flags & (FLAG_CARD_POSITION_HOLD | FLAG_CARD_DROP_HOLD)) == 0 {
    return;
  }
  let mut updated = card;
  updated.flags &= !(FLAG_CARD_POSITION_HOLD | FLAG_CARD_DROP_HOLD);
  updated.delta_t = crate::delta_t::current();
  ctx.db.cards().card_id().update(updated);
}

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
  updated.delta_t = crate::delta_t::current();
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
  /// Scheduled-reducer lag at the time of this row write, in 16-ms
  /// steps (saturating at 255). `0` for client-driven writes;
  /// non-zero only inside a scheduled reducer fire that's running
  /// late. See [`crate::delta_t`].
  pub delta_t: u8,
}

// ---------- Dead-card reaper ----------

/// Scheduled-deletion queue for cards flagged [`FLAG_CARD_DEAD`].
/// Private — clients have no reason to subscribe; the dying
/// animation is driven by the `dead` bit flip on the public `Card`
/// row, not by this table. One row per dead card.
///
/// Inserted by [`mark_card_dead`] at the time the card is flagged;
/// `scheduled_at` is `now + CARD_REAP_DELAY_SECS`. SpacetimeDB
/// fires [`reap_dead_card`] when the time arrives and removes the
/// row from this table after the reducer returns.
#[spacetimedb::table(accessor = pending_card_deletions, scheduled(reap_dead_card))]
#[derive(Debug, Clone)]
pub struct PendingCardDeletion {
  #[primary_key]
  #[auto_inc]
  pub scheduled_id: u64,
  pub scheduled_at: ScheduleAt,
  /// PK of the `Card` row to delete when this fires.
  pub card_id: u32,
}

/// Mark a card as dead — sets [`FLAG_CARD_DEAD`], stamps
/// `delta_t` (so the client can back-date its dying animation),
/// and schedules the actual row deletion for
/// `now + CARD_REAP_DELAY_SECS`. Replaces direct
/// `ctx.db.cards().card_id().delete(...)` calls everywhere in the
/// module.
///
/// Idempotent — calling twice on the same card is a no-op the
/// second time (the bit's already set; we don't schedule another
/// reap). Missing card is also a no-op.
///
/// Caller responsibility: any private bookkeeping that referenced
/// this card (today: `card_holds` keyed by `card_id`) is the
/// caller's to clean up. The `dead` UPDATE doesn't disturb private
/// rows.
pub fn mark_card_dead(ctx: &ReducerContext, card_id: u32) {
  let Some(card) = ctx.db.cards().card_id().find(&card_id) else {
    return;
  };
  if (card.flags & FLAG_CARD_DEAD) != 0 {
    return;
  }
  let mut updated = card;
  updated.flags |= FLAG_CARD_DEAD;
  updated.delta_t = crate::delta_t::current();
  ctx.db.cards().card_id().update(updated);

  let reap_at = Timestamp::from_micros_since_unix_epoch(
    ctx
      .timestamp
      .to_micros_since_unix_epoch()
      .saturating_add((CARD_REAP_DELAY_SECS as i64).saturating_mul(1_000_000)),
  );
  ctx.db.pending_card_deletions().insert(PendingCardDeletion {
    scheduled_id: 0,
    scheduled_at: ScheduleAt::Time(reap_at),
    card_id,
  });
}

/// Scheduled reducer — fires `CARD_REAP_DELAY_SECS` after a card is
/// flagged dead and removes the row. Defended against
/// client-spoofed invocation: the scheduler row must still exist
/// (legitimate fires see it; SpacetimeDB deletes it after this
/// returns). The `Card` row itself may already be gone if some
/// other path deleted it directly — `delete` on a missing PK is a
/// silent no-op, which is the right behavior here.
#[reducer]
pub fn reap_dead_card(ctx: &ReducerContext, deletion: PendingCardDeletion) -> Result<(), String> {
  if ctx
    .db
    .pending_card_deletions()
    .scheduled_id()
    .find(&deletion.scheduled_id)
    .is_none()
  {
    return Ok(());
  }
  ctx.db.cards().card_id().delete(&deletion.card_id);
  Ok(())
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
    delta_t: crate::delta_t::current(),
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

/// One stack the client is asserting as a current arrangement. The
/// position fields (`layer`, `macro_zone`, `micro_zone`,
/// `micro_location`) describe **the root's** placement; chain
/// children inherit the chain's `(layer, macro_zone)` and have their
/// `micro_zone` set to the appropriate stack-state bits with
/// `micro_location` pointing at their immediate parent.
///
/// The root's `micro_zone` low 2 bits are the stack-state:
///
/// - `0` (LOOSE) — root is loose. `micro_location` carries
///   pixel coords (world) or is unused (inventory).
/// - `1` / `2` (TOP / BOTTOM) — root is stacked on another card not
///   in this submission. `micro_location` must point to that parent.
///   (E.g. when this submission is one of multiple chains a player
///   submits in tandem after a drag.)
/// - `3` (HEX_ROOT) — root is anchored on a hex.
///   `micro_location` must point to that hex card.
///
/// Inventory layouts stay client-owned: the client ignores
/// inventory `local_q`/`local_r` unless the server forces a
/// placement via `local_q != 0`, which `submit_inventory_stacks`
/// deliberately doesn't. World layouts are server-authoritative;
/// the client mirrors whatever the server writes.
#[derive(SpacetimeType, Debug, Clone)]
pub struct InventoryStack {
  pub root: u32,
  pub layer: u8,
  pub macro_zone: u32,
  pub micro_zone: u8,
  pub micro_location: u32,
  pub stack_up: Vec<u32>,
  pub stack_down: Vec<u32>,
}

/// Client-driven stack submission. Mirrors every chain into per-card
/// row state and runs the action upgrade machinery against it.
///
/// Per-card validation: each chain card must be either in the
/// caller's inventory (`layer == LAYER_INVENTORY` and
/// `macro_zone == player_id`) or on a world layer (`layer >= LAYER_WORLD`,
/// any caller — future work will add zone proximity / permission
/// rules). Mixed-layer chains are legal — `mirror_stack` migrates
/// every member to the chain's effective location.
///
/// The chain's effective `(layer, macro_zone)` is the **hex's**
/// when `stack.hex` is set, otherwise the **root's**. This supports
/// inventory↔world transitions in either direction with no extra
/// API surface — drag a card from inventory onto a world hex to
/// migrate it up; drag it off back into a chain rooted on an
/// inventory card to migrate it down. Inventory positions stay
/// client-owned (`micro_zone` high bits zeroed; the client ignores
/// inventory positions unless the server forces a placement via
/// `local_q != 0`, which `submit_inventory_stacks` deliberately
/// doesn't).
///
/// If a stack fails server-side validation (hex set but doesn't
/// exist, or hex is in another player's inventory), the chain
/// members are returned to the caller's inventory and that stack
/// skips action processing — defensive: the client should be
/// sending valid stacks; if it isn't, we recover rather than leave
/// cards stranded.
///
/// Then per stack:
///
/// 1. **Top branch.** Build `[root, stack_up[0], …]` and call
///    [`actions::process_top_branch`]. The branch processor
///    iterates every potential actor and applies the four-way
///    upgrade decision (start / keep / refresh / cancel) — no
///    blanket pre-cancel.
/// 2. **Bottom branch.** Same shape for `[root, stack_down[0], …]`
///    via [`actions::process_bottom_branch`].
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

      // Caller authority. Mixed-layer chains are legal — `mirror_stack`
      // migrates every member to the chain's target location below.
      // Inventory cards must be in the caller's inventory; world
      // cards are interactable by anyone (future: zone proximity).
      if card.layer == LAYER_INVENTORY {
        if card.macro_zone != player_id {
          return Err(format!(
            "card {} is not in caller's inventory",
            card_id,
          ));
        }
      } else if card.layer >= LAYER_WORLD {
        // OK — world layer, any caller.
      } else {
        return Err(format!(
          "card {} on unsupported layer {}", card_id, card.layer,
        ));
      }
    }
  }

  // ─── Row sync + action orchestration ───────────────────────────────
  // For each stack: mirror the chain to row state (migrating layer +
  // macro_zone to wherever the chain effectively lives), then run the
  // upgrade machinery. A stack that fails server-side validation gets
  // its members pulled back to the caller's inventory and skips
  // action processing.
  for stack in &stacks {
    if !mirror_stack(ctx, stack, player_id)? {
      // Validation failed; chain returned to inventory. Skip this
      // stack's action machinery — there's nothing left to match.
      continue;
    }

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

/// Mirror a submitted stack into per-card row state. Returns
/// `Ok(true)` if mirrored cleanly; `Ok(false)` if the stack failed
/// server-side validation and the chain was returned to the caller's
/// inventory instead.
///
/// The submission carries the **root's** full row state
/// (`layer`, `macro_zone`, `micro_zone`, `micro_location`); we copy
/// it verbatim onto the root's row. Children inherit the chain's
/// `(layer, macro_zone)` and get their `micro_zone` set to the
/// appropriate stack-state bits with `micro_location` pointing at
/// their immediate parent.
///
/// **Validation**:
///
/// - Target authority — when `stack.layer == LAYER_INVENTORY`,
///   `stack.macro_zone` must be the caller's `player_id`.
///   World-layer targets are open to any caller (future: zone
///   proximity / permission rules). Failures route the chain to
///   caller's inventory.
/// - Stack-state normalization — when the root's `micro_zone` low
///   2 bits indicate a parent (`1`/`2`/`3`) but `micro_location`
///   doesn't resolve, the state bits are cleared and
///   `micro_location` is zeroed. Stale client-local state (e.g.
///   the parent vanished after the player last touched the card)
///   gets cleaned up rather than rejected — the matcher's hex
///   resolver handles missing-hex chains gracefully on its own.
fn mirror_stack(
  ctx: &ReducerContext,
  stack: &InventoryStack,
  player_id: u32,
) -> Result<bool, String> {
  use crate::magnetic::{STACK_STATE_BOTTOM, STACK_STATE_TOP};

  // Target authority. Inventory targets must be the caller's; world
  // targets are open to anyone; other layers are nonsense.
  let target_authorized = match stack.layer {
    LAYER_INVENTORY => stack.macro_zone == player_id,
    l if l >= LAYER_WORLD => true,
    _ => false,
  };
  if !target_authorized {
    return_chain_to_inventory(ctx, stack, player_id);
    return Ok(false);
  }

  // Stack-state normalization. States 1/2/3 imply `micro_location`
  // points at a real card (parent rect or hex); when it doesn't —
  // typically a stale client-local state bit after the parent
  // vanished — clamp the state to LOOSE rather than bail. The
  // alternative (returning the chain to inventory) was too eager,
  // killing recipe processing for chains the client otherwise
  // matched cleanly.
  let raw_state = stack.micro_zone & 0b11;
  let parent_resolves = stack.micro_location != 0
    && ctx
      .db
      .cards()
      .card_id()
      .find(&stack.micro_location)
      .is_some();
  // State 3 (HEX_ROOT) with micro_location == 0 on a world layer is the
  // valid "bare tile" convention — position is encoded in micro_zone's high
  // bits, not via a parent card. Do not treat this as a dangling reference.
  let bare_world_tile =
    raw_state == 3 && stack.micro_location == 0 && stack.layer >= LAYER_WORLD;
  let (root_micro_zone, root_micro_location) = if raw_state != 0 && !parent_resolves && !bare_world_tile {
    // Parent card referenced but doesn't exist — strip dangling state bits.
    (stack.micro_zone & !0b11, 0)
  } else {
    (stack.micro_zone, stack.micro_location)
  };

  // Root: write the (normalized) client-supplied row state.
  {
    let mut root = ctx
      .db
      .cards()
      .card_id()
      .find(&stack.root)
      .ok_or_else(|| format!("mirror_stack: root {} missing", stack.root))?;
    root.layer = stack.layer;
    root.macro_zone = stack.macro_zone;
    root.micro_zone = root_micro_zone;
    root.micro_location = root_micro_location;
    if stack.layer == LAYER_INVENTORY {
      root.flags &= !(FLAG_CARD_POSITION_HOLD | FLAG_CARD_DROP_HOLD);
    }
    root.delta_t = crate::delta_t::current();
    ctx.db.cards().card_id().update(root);
  }

  // Up branch.
  let mut parent_id = stack.root;
  for &child_id in &stack.stack_up {
    update_chain_member(ctx, child_id, parent_id, stack.layer, stack.macro_zone, STACK_STATE_TOP)?;
    parent_id = child_id;
  }

  // Down branch.
  let mut parent_id = stack.root;
  for &child_id in &stack.stack_down {
    update_chain_member(ctx, child_id, parent_id, stack.layer, stack.macro_zone, STACK_STATE_BOTTOM)?;
    parent_id = child_id;
  }
  Ok(true)
}

/// Migrate one chain member's row to the chain's target location and
/// parent. Stacked cards have no meaningful `local_q`/`local_r`
/// (their visual position is derived from the parent), so the high
/// `micro_zone` bits go to zero. Inventory landings clear the
/// magnetic hold flags — the card is "back in inventory" and any
/// previous placement-bound holds are stale.
fn update_chain_member(
  ctx: &ReducerContext,
  card_id: u32,
  parent_id: u32,
  target_layer: u8,
  target_macro_zone: u32,
  stack_state: u8,
) -> Result<(), String> {
  let mut card = ctx
    .db
    .cards()
    .card_id()
    .find(&card_id)
    .ok_or_else(|| format!("mirror_stack: chain card {} missing", card_id))?;
  card.layer = target_layer;
  card.macro_zone = target_macro_zone;
  card.micro_zone = stack_state;
  card.micro_location = parent_id;
  if target_layer == LAYER_INVENTORY {
    card.flags &= !(FLAG_CARD_POSITION_HOLD | FLAG_CARD_DROP_HOLD);
  }
  card.delta_t = crate::delta_t::current();
  ctx.db.cards().card_id().update(card);
  Ok(())
}

/// Recovery path for invalid stacks — pulls every chain member back
/// to the caller's inventory at a known-good loose state and clears
/// magnetic hold flags. The hex card (if any) is left alone; only
/// chain members are touched.
fn return_chain_to_inventory(
  ctx: &ReducerContext,
  stack: &InventoryStack,
  player_id: u32,
) {
  use crate::magnetic::STACK_STATE_LOOSE;
  let cards_in_stack = std::iter::once(stack.root)
    .chain(stack.stack_up.iter().copied())
    .chain(stack.stack_down.iter().copied());
  for card_id in cards_in_stack {
    if let Some(mut card) = ctx.db.cards().card_id().find(&card_id) {
      card.layer = LAYER_INVENTORY;
      card.macro_zone = player_id;
      card.micro_zone = STACK_STATE_LOOSE;
      card.micro_location = 0;
      card.flags &= !(FLAG_CARD_POSITION_HOLD | FLAG_CARD_DROP_HOLD);
      card.delta_t = crate::delta_t::current();
      ctx.db.cards().card_id().update(card);
    }
  }
}
