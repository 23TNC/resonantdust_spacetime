use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, ReducerContext};

use crate::cards;
use crate::packed::{pack_macro_zone, pack_tiles, pack_zone_definition};
use crate::players;
use crate::zones;

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
    crate::on_create::trigger(ctx, card_id, player_id)?;

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
/// - Three world zones at hex coordinates `(0, 0)`, `(0, -1)`, and
///   `(-1, 0)`. Each zone's tiles are filled with `def_id = 1` (every
///   row encodes `0x01` repeated eight times), and the zone itself is
///   tagged with `packed_definition = pack_zone_definition(6, 0)`
///   (`card_type = 6`, `card_category = 0`).
///
/// Zone IDs are hard-coded `1`, `2`, `3` for the three coords in
/// declaration order. Re-running `bootstrap` adds a new `valid_at`
/// version per zone_id (same row history mechanics as cards), so it
/// won't crash but will accumulate zone versions — intended as a
/// one-shot seed, not a repeated call.
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
        crate::on_create::trigger(ctx, card_id, player_id)?;
    }

    // ---- 3 zones at (0, 0), (0, -1), (-1, 0) ------------------------
    //
    // Tiles are 8 rows of 8 bytes each, every byte = def_id 1.
    // `pack_tiles([1u8; 8])` packs one row; we use the same row for all
    // eight rows of every zone.
    let zone_coords: [(i16, i16); 3] = [(0, 0), (0, -1), (-1, 0)];
    let zone_packed_def: u8 = pack_zone_definition(/* card_type */ 7, /* card_category */ 0);
    let tile_row: u64 = pack_tiles([1u8; 8]);
    let tiles: [u64; 8] = [tile_row; 8];
    for (i, (q, r)) in zone_coords.iter().enumerate() {
        let zone_id = (i + 1) as u32;
        let macro_zone = pack_macro_zone(*q, *r);
        zones::create(
            ctx,
            zone_id,
            /* surface */ 64,
            macro_zone,
            zone_packed_def,
            tiles,
        );
    }

    Ok(())
}
