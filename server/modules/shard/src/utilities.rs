use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, ReducerContext};

use crate::cards::{self, cards as _cards_table};
use crate::packed::{
    micro_zone_direction, pack_micro_location_xy, pack_micro_zone, pack_slot_micro_zone,
    pack_stack_micro_zone, unpack_micro_zone, StackedState, INVENTORY_LAYER, STACK_DIR_UP,
};
use crate::players;
use crate::recipe_eval::soul_stack;
use crate::world_gen;

// `cards/flags.json` bit positions. Local to keep the file self-
// contained; same pattern as the other modules.
const FLAG_DEAD: u32 = 1 << 7;
const FLAG_SLOT_HOLD: u32 = 1 << 5;

fn now_ms(ctx: &ReducerContext) -> u64 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64
}

/// Hex-disk radius around macro `(0, 0)` to generate on bootstrap.
/// Radius 2 → 19 zones, enough for the starter area to span multiple
/// forest blobs. Tune as the playable area grows.
///
/// The world seed itself lives in `world_gen::WORLD_SEED` — moved
/// there because `action_completion::apply` also keys off it to
/// revert consumed-tile bytes back to the underlying biome, so both
/// generation and revert must agree on the same value.
const BOOTSTRAP_WORLD_RADIUS: i16 = 2;

/// Add a single card to a specific soul's inventory bucket.
///
/// Dev/admin tool — invoked from the CLI, not the client. The
/// production client uses `propose_action` and never calls this. No
/// caller-identity check: the CLI sends an anonymous identity that
/// doesn't resolve to a player. The acting player for on-create
/// downstream is derived from `soul_card_id`'s `owning_player`
/// instead.
///
/// `card_key` is the bare key from the card definition catalog (e.g.
/// `"attack"`, `"fatigue"`) — the same identifier used in
/// `content/cards/id.json`. It's resolved to a `packed_definition` via
/// `resonantdust_content::definition_core::find_packed_by_key`. Pass the
/// path-form `"type/category/key"` and you'll get a "unknown card key" error
/// — use the bare key here.
///
/// Card placement uses the inventory convention:
/// - `surface = 1` (inventory surface)
/// - `macro_zone = soul_card_id` (each soul has its own inventory bucket)
/// - `micro_zone = 0` (q=0, r=0, stacked_state=Free — i.e. loose, not stacked)
/// - `micro_location = 0` (top-left for now; layout is the client's concern)
/// - `owner_id = soul_card_id` (the soul is the inventory's container card)
/// - `flags = 0` (NOT `FLAG_OWNED_BY_PLAYER` — owner_id is a card_id here)
///
/// `card_id` is allocated by scanning the cards table for the highest
/// existing `card_id` and adding 1 — same pattern as `players::next_player_id`.
/// O(N) over the cards history; fine while the table is small.
#[reducer]
pub fn add_card(
    ctx: &ReducerContext,
    soul_card_id: u32,
    card_key: String,
) -> Result<(), String> {
    let packed_definition = find_packed_by_key(&card_key)?
        .ok_or_else(|| format!("unknown card key {:?}", card_key))?;

    let soul_player = cards::owning_player(ctx, soul_card_id).ok_or_else(|| {
        format!("add_card: soul card {soul_card_id} not found or world-owned")
    })?;

    let card_id = cards::next_card_id(ctx);

    cards::create(
        ctx,
        card_id,
        /* surface         */ 1,
        /* macro_zone      */ soul_card_id,
        /* micro_zone      */ 0,
        /* micro_location  */ 0,
        /* owner_id        */ soul_card_id,
        packed_definition,
        /* flags           */ 0,
    );

    // OnCreate recipe matching has moved client-side: when a card is
    // spawned, the client scans root-only recipes against it and
    // submits a `propose_action` if any apply. The server no longer
    // auto-triggers anything on card creation.
    let _ = soul_player;

    Ok(())
}

/// Seed the world's terrain.
///
/// Delegates to [`world_gen::generate_forest_terrain`] against
/// `world_gen::WORLD_SEED` over a `BOOTSTRAP_WORLD_RADIUS` hex disk
/// around macro `(0, 0)`. Idempotent on re-runs: zone-tile bytes are
/// deterministic (so the second call regenerates identical rows),
/// and the world-card spawn path skips tiles already holding a world
/// card.
///
/// **No per-player setup happens here anymore.** Soul + starter
/// cards are spawned by `character_creation::create_character` when
/// the player picks a starter pack at character select. This reducer
/// is purely a world-seeding entry point (admin / dev tooling).
///
/// **Surface convention:** zone rows use `surface = 64` (first world
/// layer; the `< 64` range is reserved for inventory-ish surfaces,
/// see the q=1 force rule discussion in `actions.rs`).
#[reducer]
pub fn bootstrap(ctx: &ReducerContext) -> Result<(), String> {
    world_gen::generate_forest_terrain(ctx, world_gen::WORLD_SEED, BOOTSTRAP_WORLD_RADIUS)?;
    Ok(())
}

/// Equip a card onto a soul's UP-stack (equipment branch).
///
/// The target soul is derived from the card itself via
/// `cards::owning_soul(card_id)` — equip moves the card into a
/// chain rooted at the soul that *currently contains it*. That
/// means the card must already be in a soul's inventory bucket
/// (or be a loose card whose `owner_id` points at one). World-
/// owned cards (`owning_soul` returns `None`) are rejected — pick
/// the card up into your inventory first, then equip.
///
/// The card becomes a chain-stitched child of the deepest existing
/// equipped card (or directly on the soul, if the stack is empty),
/// in the UP direction. After equipping, the card sits in the
/// soul's "equipment" position and is visible to has-predicate
/// resolution (see `recipe_eval::soul_stack`).
///
/// **Surface/macro_zone are copied from the soul.** A card dragged
/// in from the world (`surface = 64`, `macro_zone = world chunk`)
/// must move to the soul's address so it sits inside the inventory
/// subscription scope — otherwise the client mirroring inventory
/// can't see it and `recipe_eval::soul_stack` traversal would walk
/// off into a zone nobody's subscribed to. Matches the chain-stitch
/// convention in `actions.rs` (every slot inherits the anchor's
/// `surface` + `macro_zone`).
///
/// **Eligibility:**
/// - Card must exist, ultimately belong to the calling player (via
///   `cards::owning_player`), not be a soul itself, not be `dead`,
///   not be `slot_held` (claimed by an in-flight action), and not
///   currently in a chain (`state` is `Free`). Already-chained cards
///   are rejected to avoid silently dropping their children.
///
/// **Stack layout written:**
/// - First equipped card (stack empty): `state = OnRoot`,
///   `micro_location = soul_card_id`, `micro_zone = pack_stack_micro_zone(
///   position=1, direction=UP, OnRoot)`.
/// - Subsequent cards: `state = Slot`, `micro_location = top_of_stack_id`,
///   `micro_zone = pack_slot_micro_zone(UP)` — same encoding the
///   `propose_action` slot[1..] loop writes.
///
/// Wall-clock-stamped via `cards::update_with` so the equip takes
/// effect immediately.
#[reducer]
pub fn equip_card(
    ctx: &ReducerContext,
    card_id: u32,
) -> Result<(), String> {
    let caller_player_id = players::resolve_caller(ctx)?;
    let card = cards::latest(ctx, card_id)
        .ok_or_else(|| format!("equip_card: card {card_id} not found"))?;

    // Walk the card up to its soul. World-owned cards (no soul in
    // their chain) can't be equipped without first being picked up
    // into the caller's inventory.
    let soul_card_id = cards::owning_soul(ctx, card_id).ok_or_else(|| {
        format!(
            "equip_card: card {card_id} is not in any soul's inventory (world-owned or orphaned)"
        )
    })?;
    // Ownership: the soul must belong to the caller.
    let soul_player = cards::owning_player(ctx, soul_card_id).unwrap_or(cards::WORLD_PLAYER_ID);
    if soul_player != caller_player_id {
        return Err(format!(
            "equip_card: card {card_id}'s soul {soul_card_id} is owned by player {soul_player} (not {caller_player_id})"
        ));
    }
    if card_id == soul_card_id {
        return Err(format!("equip_card: can't equip soul card {card_id} onto itself"));
    }
    let soul = cards::latest(ctx, soul_card_id)
        .ok_or_else(|| format!("equip_card: soul card {soul_card_id} not found"))?;
    if card.flags & FLAG_DEAD != 0 {
        return Err(format!("equip_card: card {card_id} is dead"));
    }
    if card.flags & FLAG_SLOT_HOLD != 0 {
        return Err(format!(
            "equip_card: card {card_id} is already claimed by an in-flight action"
        ));
    }
    // Reject cards already in a chain — equipping them would orphan
    // any descendants whose `micro_location` points back at this card.
    // Only `Free` cards are eligible; the player must first pull a
    // chained card loose before equipping.
    let (_, _, current_state) = unpack_micro_zone(card.micro_zone);
    if matches!(current_state, StackedState::OnRoot | StackedState::Slot) {
        return Err(format!(
            "equip_card: card {card_id} is already part of a chain (state {current_state:?}); \
             pull it loose first"
        ));
    }

    // Walk the existing UP stack to find where to attach. BFS order
    // yields the chain in depth order; the *last* entry is the
    // current top — that's the new card's parent (or the soul, if
    // the stack is empty).
    let stack = soul_stack(ctx, soul_card_id, STACK_DIR_UP);
    let (parent_id, new_micro_zone) = match stack.last() {
        Some(top) => (
            top.card_id,
            pack_slot_micro_zone(STACK_DIR_UP),
        ),
        None => (
            soul_card_id,
            // First card on the soul: state=OnRoot, position=1.
            pack_stack_micro_zone(1, STACK_DIR_UP, StackedState::OnRoot),
        ),
    };

    cards::update_with(ctx, card_id, |c| {
        c.surface = soul.surface;
        c.macro_zone = soul.macro_zone;
        c.micro_location = parent_id;
        c.micro_zone = new_micro_zone;
        // Under the post-flag-20 card-owner model, an equipped card
        // sits in the soul's inventory bucket — `owner_id` carries
        // the soul's card_id. (FLAG_OWNED_BY_PLAYER stays clear; the
        // soul itself is the only `is_owned_by_player` card in the
        // chain.) Re-stamping `owner_id` on every equip handles the
        // case where the source card came from the world or from a
        // different soul's inventory.
        c.owner_id = soul_card_id;
    });

    Ok(())
}

/// Unequip a card from a soul's UP-stack — inverse of [`equip_card`].
///
/// The card AND everything currently above it in the chain detach as
/// a single sub-chain into the soul's inventory bucket. The unequipped
/// card becomes `Free` (loose) at `(target_x, target_y)`; descendants
/// keep their parent-pointer back to it (a `Free`-rooted chain is a
/// normal inventory stack shape). Descendants are re-stamped onto the
/// inventory surface so they stay inside the soul's subscription scope.
///
/// **Mid-chain semantics:** unequipping a card with cards stacked
/// above it pulls all of them along. Matches the drag UX where
/// grabbing a chain member detaches the rest with it.
///
/// **Eligibility:**
/// - Card must exist; `owning_soul` must resolve; soul must belong to
///   the caller.
/// - Card must currently be `OnRoot` or `Slot` in the UP direction.
/// - Neither the card nor any descendant in its sub-chain may carry
///   `FLAG_SLOT_HOLD` (rejecting mid-recipe unequip).
#[reducer]
pub fn unequip_card(
    ctx: &ReducerContext,
    card_id: u32,
    target_x: i16,
    target_y: i16,
) -> Result<(), String> {
    let caller_player_id = players::resolve_caller(ctx)?;
    let card = cards::latest(ctx, card_id)
        .ok_or_else(|| format!("unequip_card: card {card_id} not found"))?;

    let soul_card_id = cards::owning_soul(ctx, card_id).ok_or_else(|| {
        format!("unequip_card: card {card_id} has no owning soul (world-owned or orphaned)")
    })?;
    let soul_player = cards::owning_player(ctx, soul_card_id).unwrap_or(cards::WORLD_PLAYER_ID);
    if soul_player != caller_player_id {
        return Err(format!(
            "unequip_card: card {card_id}'s soul {soul_card_id} is owned by player {soul_player} (not {caller_player_id})"
        ));
    }
    if card_id == soul_card_id {
        return Err(format!("unequip_card: can't unequip soul card {card_id}"));
    }

    // Idempotency: if the card is already Free, the unequip target
    // state is already achieved. The client's drop path computes
    // `sourceWasEquipped` from the LOCAL row, which can drift from
    // the server when `mirrorCard.preservePosition` keeps a stale
    // equipped state across a server-side Free push (FORCE_POSITION
    // is clear on equip/release writes, so position-preserve fires).
    // Returning Ok keeps the user's drag UX smooth — the row is just
    // re-positioned at (target_x, target_y) so the visual matches
    // wherever they dropped it.
    let (_, _, state) = unpack_micro_zone(card.micro_zone);
    if matches!(state, StackedState::Free) {
        cards::update_with(ctx, card_id, |c| {
            c.surface = INVENTORY_LAYER;
            c.macro_zone = soul_card_id;
            c.micro_zone = pack_micro_zone(0, 0, StackedState::Free);
            c.micro_location = pack_micro_location_xy(target_x, target_y);
            c.owner_id = soul_card_id;
        });
        return Ok(());
    }
    if !matches!(state, StackedState::OnRoot | StackedState::Slot) {
        return Err(format!(
            "unequip_card: card {card_id} is in unexpected state {state:?}"
        ));
    }
    // Direction filter: this reducer is for the UP (equipment) chain.
    // Action-stack (DOWN) cards aren't player-detachable.
    if micro_zone_direction(card.micro_zone) != STACK_DIR_UP {
        return Err(format!(
            "unequip_card: card {card_id} is not on the UP (equipment) chain"
        ));
    }
    if card.flags & FLAG_SLOT_HOLD != 0 {
        return Err(format!(
            "unequip_card: card {card_id} is claimed by an in-flight action"
        ));
    }
    // `position_hold` (ref-counted at bits 17..=19) is the broader
    // movement-block: any in-flight recipe whose path resolves to
    // this card sets it, even when `slot_hold` is off (e.g. a
    // `borrow.` nested-iterator predicate like `cut_tree`'s axe).
    // Unequipping a position-held card would shift it out of the
    // chain the recipe's path expects.
    if cards::position_hold_count(card.flags) > 0 {
        return Err(format!(
            "unequip_card: card {card_id} is position-held by an in-flight action"
        ));
    }

    // Collect descendants (sub-chain above `card_id`). Any descendant
    // carrying FLAG_SLOT_HOLD or position_hold blocks the unequip —
    // detaching it would orphan it mid-recipe (slot_hold) or shift
    // it out of an in-flight recipe's path (position_hold).
    let descendants = chain_descendants(ctx, soul_card_id, card_id);
    for d in &descendants {
        if d.flags & FLAG_SLOT_HOLD != 0 {
            return Err(format!(
                "unequip_card: descendant card {} is claimed by an in-flight action",
                d.card_id
            ));
        }
        if cards::position_hold_count(d.flags) > 0 {
            return Err(format!(
                "unequip_card: descendant card {} is position-held by an in-flight action",
                d.card_id
            ));
        }
    }

    // Detach this card → Free in inventory at the supplied target xy.
    cards::update_with(ctx, card_id, |c| {
        c.surface = INVENTORY_LAYER;
        c.macro_zone = soul_card_id;
        c.micro_zone = pack_micro_zone(0, 0, StackedState::Free);
        c.micro_location = pack_micro_location_xy(target_x, target_y);
        c.owner_id = soul_card_id;
    });

    // Re-stamp descendants into the inventory bucket. They keep their
    // existing micro_zone / micro_location (parent-pointer to `card_id`
    // is still valid; the chain shape is "Free root + Slot children",
    // which is exactly a normal inventory stack). Only surface and
    // macro_zone need updating because equip_card put them on the
    // soul's world surface.
    for d in &descendants {
        cards::update_with(ctx, d.card_id, |c| {
            c.surface = INVENTORY_LAYER;
            c.macro_zone = soul_card_id;
            c.owner_id = soul_card_id;
        });
    }

    Ok(())
}

/// BFS-walk the cards owned by `soul_card_id` to collect the sub-chain
/// rooted at `start_card_id` — every card whose chain of parent-
/// pointers (`micro_location`) walks back through `start_card_id`.
/// The starting card itself is NOT included.
///
/// Used by `unequip_card` to (1) pre-validate that no descendant
/// carries `FLAG_SLOT_HOLD` and (2) re-stamp surface / macro_zone
/// after detach. Mirrors the BFS shape of `recipe_eval::soul_stack`
/// but starts from an arbitrary chain member rather than the soul,
/// and intentionally skips the state / dead / slot_hold filters so
/// callers can inspect those for themselves.
fn chain_descendants(
    ctx: &ReducerContext,
    soul_card_id: u32,
    start_card_id: u32,
) -> Vec<cards::Card> {
    use std::collections::{BTreeMap, BTreeSet};
    const DEPTH_CAP: usize = 32;

    // Build parent → children map from soul-owned cards. Iteration via
    // the owner_id btree index keeps this cheap; same access pattern
    // as soul_stack.
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut children_of: BTreeMap<u32, Vec<cards::Card>> = BTreeMap::new();
    for row in ctx.db.cards().owner_id().filter(soul_card_id) {
        if !seen.insert(row.card_id) {
            continue;
        }
        let Some(latest) = cards::latest(ctx, row.card_id) else {
            continue;
        };
        if latest.micro_location == 0 {
            continue;
        }
        children_of
            .entry(latest.micro_location)
            .or_default()
            .push(latest);
    }

    let mut out: Vec<cards::Card> = Vec::new();
    let mut frontier: Vec<u32> = vec![start_card_id];
    for _ in 0..DEPTH_CAP {
        let mut next: Vec<u32> = Vec::new();
        for parent in &frontier {
            if let Some(children) = children_of.remove(parent) {
                for child in children {
                    next.push(child.card_id);
                    out.push(child);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}
