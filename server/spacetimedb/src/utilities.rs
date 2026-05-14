use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, ReducerContext};

use crate::cards;
use crate::packed::{
    pack_slot_micro_zone, pack_stack_micro_zone, unpack_micro_zone, StackedState, STACK_DIR_UP,
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

/// Add a single card to a player's inventory.
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
/// - `macro_zone = player_id` (the inventory's macro_zone is the owner's id)
/// - `micro_zone = 0` (q=0, r=0, stacked_state=Free — i.e. loose, not stacked)
/// - `micro_location = 0` (top-left for now; layout is the client's concern)
/// - `owner_id = player_id`
/// - `flags = 0`
///
/// `card_id` is allocated by scanning the cards table for the highest
/// existing `card_id` and adding 1 — same pattern as `players::next_player_id`.
/// O(N) over the cards history; fine while the table is small.
#[reducer]
pub fn add_card(
    ctx: &ReducerContext,
    player_id: u32,
    card_key: String,
) -> Result<(), String> {
    let packed_definition = find_packed_by_key(&card_key)?
        .ok_or_else(|| format!("unknown card key {:?}", card_key))?;

    // Don't let callers add cards owned by a player that doesn't exist —
    // would leave the cards table with orphan rows whose owner_id points
    // at no one.
    if players::latest(ctx, player_id).is_none() {
        return Err(format!("player {player_id} not found"));
    }

    let card_id = cards::next_card_id(ctx);

    cards::create(
        ctx,
        card_id,
        /* surface         */ 1,
        /* macro_zone      */ player_id,
        /* micro_zone      */ 0,
        /* micro_location  */ 0,
        /* owner_id        */ player_id,
        packed_definition,
        /* flags           */ 0,
    );

    // Run OnCreate recipe matching against the new card. If a recipe
    // matches, holds get stamped on the card's row and a completion is
    // scheduled (`action_completion::apply` at card.valid_at + duration).
    crate::on_create::trigger(ctx, card_id, player_id, now_ms(ctx))?;

    Ok(())
}

/// Seed the database with starter content for a given player.
///
/// Creates:
///
/// - Three `corpus` cards in `player_id`'s inventory. Goes through the
///   same `cards::create` + `on_create::trigger` path as `add_card`, so
///   OnCreate recipe matching fires (e.g. `fleeting` if a card carries
///   the matching aspect).
/// - World terrain via [`world_gen::generate_forest_terrain`] against
///   `world_gen::WORLD_SEED` over a `BOOTSTRAP_WORLD_RADIUS` hex disk
///   around macro `(0, 0)`.
///
/// World-gen is idempotent on re-runs: zone-tile bytes are
/// deterministic (so the second-player bootstrap regenerates identical
/// rows), and the world-card spawn path skips tiles already holding a
/// world card. The corpus-card path is NOT idempotent — every
/// bootstrap call adds three more corpus cards to the inventory, since
/// `card_id`s are unique per allocation.
///
/// **Surface convention:** zone rows use `surface = 64` (first world
/// layer; the `< 64` range is reserved for inventory-ish surfaces, see
/// the q=1 force rule discussion in `actions.rs`). Corpus cards use
/// `surface = 1` per the inventory convention shared with `add_card`.
#[reducer]
pub fn bootstrap(ctx: &ReducerContext, player_id: u32) -> Result<(), String> {
    if players::latest(ctx, player_id).is_none() {
        return Err(format!("player {player_id} not found"));
    }

    // ---- 3 corpus cards ---------------------------------------------
    let corpus_def = find_packed_by_key("corpus")
        .map_err(|e| format!("bootstrap: lookup corpus def: {e}"))?
        .ok_or_else(|| "bootstrap: corpus def not registered".to_string())?;
    for _ in 0..3 {
        let card_id = cards::next_card_id(ctx);
        cards::create(
            ctx,
            card_id,
            /* surface         */ 1,
            /* macro_zone      */ player_id,
            /* micro_zone      */ 0,
            /* micro_location  */ 0,
            /* owner_id        */ player_id,
            corpus_def,
            /* flags           */ 0,
        );
        crate::on_create::trigger(ctx, card_id, player_id, now_ms(ctx))?;
    }

    // ---- world terrain ---------------------------------------------
    //
    // Delegating to the world-gen reducer keeps the seed / radius /
    // tree-rock-spawn logic in one place. The reducer also auto-creates
    // the World pseudo-player on first call, so trees and rocks land
    // owned by a valid Player row.
    world_gen::generate_forest_terrain(ctx, world_gen::WORLD_SEED, BOOTSTRAP_WORLD_RADIUS)?;

    Ok(())
}

/// Equip a card onto a player's soul UP-stack (equipment branch).
///
/// The card becomes a chain-stitched child of the deepest existing
/// equipped card (or directly on the soul, if the stack is empty),
/// in the UP direction. After equipping, the card sits in the
/// player's "equipment" position and is visible to has-predicate
/// resolution (see `recipe_eval::soul_stack`).
///
/// **Surface/macro_zone are intentionally left unchanged.** The
/// chain is a logical parent-pointer relationship via `micro_location`;
/// the physical (surface, macro_zone) only affects client rendering
/// and subscription scope. Today every callsite leaves the equipped
/// card on its inventory address (surface=1, macro_zone=player_id);
/// the server identifies "equipped" purely via the chain shape.
///
/// **Eligibility:**
/// - Player must exist and have a soul card.
/// - Card must exist, be owned by the player, not be the soul
///   itself, not be `dead`, not be `slot_held` (claimed by an
///   in-flight action), and not currently in a chain (`state` is
///   `Free` or `OnHex`). Already-chained cards are rejected to
///   avoid silently dropping their children.
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
    player_id: u32,
    card_id: u32,
) -> Result<(), String> {
    let player = players::latest(ctx, player_id)
        .ok_or_else(|| format!("equip_card: player {player_id} not found"))?;
    if player.soul_card_id == 0 {
        return Err(format!(
            "equip_card: player {player_id} has no soul card to equip onto"
        ));
    }

    let card = cards::latest(ctx, card_id)
        .ok_or_else(|| format!("equip_card: card {card_id} not found"))?;
    if card.owner_id != player_id {
        return Err(format!(
            "equip_card: card {card_id} is owned by player {} (not {player_id})",
            card.owner_id
        ));
    }
    if card_id == player.soul_card_id {
        return Err(format!("equip_card: can't equip soul card {card_id} onto itself"));
    }
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
    // Only Free / OnHex cards are eligible; the player must first
    // pull a chained card loose before equipping.
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
    let stack = soul_stack(ctx, player.soul_card_id, player_id, STACK_DIR_UP);
    let (parent_id, new_micro_zone) = match stack.last() {
        Some(top) => (
            top.card_id,
            pack_slot_micro_zone(STACK_DIR_UP),
        ),
        None => (
            player.soul_card_id,
            // First card on the soul: state=OnRoot, position=1.
            pack_stack_micro_zone(1, STACK_DIR_UP, StackedState::OnRoot),
        ),
    };

    cards::update_with(ctx, card_id, |c| {
        c.micro_location = parent_id;
        c.micro_zone = new_micro_zone;
    });

    Ok(())
}
