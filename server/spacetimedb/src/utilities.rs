use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, ReducerContext};

use crate::cards;
use crate::players;

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

    Ok(())
}
