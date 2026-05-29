use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, ReducerContext};

use crate::cards;

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
/// - `micro = Loose` at cell (0,0), loose-rect (inventory grid; top-left for
///   now, layout is the client's concern)
/// - `owner_id = soul_card_id` (the soul is the inventory's container card)
/// - `flags = 0` (NOT `FLAG_OWNED_BY_PLAYER` — owner_id is a card_id here)
///
/// `card_id` is allocated by scanning the cards table for the highest
/// existing `card_id` and adding 1 — same pattern as `players::next_player_id`.
/// O(N) over the cards history; fine while the table is small.
#[reducer]
pub fn add_card(
    ctx: &ReducerContext,
    client_time_ms: u64,
    soul_card_id: u32,
    card_key: String,
) -> Result<(), String> {
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;
    let packed_definition = find_packed_by_key(&card_key)?
        .ok_or_else(|| format!("unknown card key {:?}", card_key))?;

    let soul_player = cards::owning_player(ctx, soul_card_id).ok_or_else(|| {
        format!("add_card: soul card {soul_card_id} not found or world-owned")
    })?;

    let card_id = cards::next_card_id(ctx);

    cards::create_at(
        ctx,
        card_id,
        now_ms,
        /* macro_zone      */ crate::packed::pack_macro_zone_full(soul_card_id, 1, 0, 0),
        /* micro           */ cards::Micro::snap(0, 0, crate::packed::LOOSE_RECT),
        /* owner_id        */ soul_card_id,
        packed_definition,
        /* flags_state     */ 0,
        /* flags_bk        */ 0,
    );

    // OnCreate recipe matching has moved client-side: when a card is
    // spawned, the client scans root-only recipes against it and
    // submits a `propose_action` if any apply. The server no longer
    // auto-triggers anything on card creation.
    let _ = soul_player;

    Ok(())
}

