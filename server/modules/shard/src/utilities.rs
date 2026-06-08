use spacetimedb::{reducer, ReducerContext, Table};

use crate::cards;
use crate::souls::{is_soul_card, soul_privates as _soul_privates_table, SoulPrivate};

/// Generic card-creation primitive — the single explicit way to mint a card
/// owned by something. The whole entity tree is built by **chaining** it, each
/// step reading back the new card's id (via the caller's subscriptions) to use
/// as the next `owner_id`:
///
/// ```text
///   player       --create_card(owner=player_id,   def=player_soul, PLAYER_SOUL)--> player_soul
///   player_soul  --create_card(owner=player_soul, def=human,       WORLD)------->  world soul
///   world soul   --create_card(owner=soul,        def=corpus/…,    INVENTORY)--->  cards
/// ```
///
/// This **decouples player from soul** — there is no "spawn THE player's soul on
/// login" coupling — and naturally supports multiple souls / characters per
/// player (just chain more). Args:
/// - `owner_id`     — a `player_id` (for a player_soul) or a `card_id` otherwise.
/// - `surface`      — `PLAYER_SOUL_SURFACE`(0) / `WORLD_LAYER`(64) place the card
///                    in that band's chunk (0,0); `INVENTORY_LAYER`(1) places it
///                    in `owner_id`'s inventory bucket.
/// - `packed_definition` — gate-resolved content def (gate maps name → packed).
///   A player_soul is identified by its DEFINITION (the reserved range
///   `>= 0xFFF0`), so no `player_owned` flag is set here anymore.
///
/// The card write triggers `souls::on_card_write`, which auto-creates the `Soul`
/// row for soul cards; this also seeds an empty `SoulPrivate` for them.
/// **Authorization is the gateway's job** (this trusts its args) — a dev/seed +
/// future-registration primitive, not the gameplay path (`propose_action`).
#[reducer]
pub fn create_card(
    ctx: &ReducerContext,
    // `client_time_ms` (no underscore): SpacetimeDB `/call` keys on the exact
    // Rust param name, so the gate/client must be able to address it.
    client_time_ms: u64,
    owner_id: u32,
    surface: u8,
    // Gate-supplied packed def (the gate resolves the content name → packed).
    packed_definition: u16,
    // Gate-supplied initial per-instance stock u32 (the def's `@define` stock
    // defaults; the content-agnostic shard can't derive them).
    stock: u32,
    // Optional placement override (caller decomposes world coords → zone+cell):
    // `macro_zone != 0` ⇒ spawn there at loose cell `(q, r)`; `0` ⇒ default
    // (inventory bucket / the surface's (0,0) cell). World/inventory zones are
    // never 0, so 0 is an unambiguous "no override" sentinel.
    macro_zone: u64,
    q: u8,
    r: u8,
) -> Result<(), String> {
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;

    // Default placement: inventory cards in their owner's bucket; everything else
    // in its surface band's (0,0) cell. An explicit `macro_zone` override wins.
    let (macro_zone, micro) = if macro_zone != 0 {
        (macro_zone, cards::Micro::snap(q, r))
    } else if surface == crate::packed::INVENTORY_LAYER {
        (
            crate::packed::pack_macro_zone_full(owner_id, crate::packed::INVENTORY_LAYER, 0, 0),
            cards::Micro::snap(0, 0),
        )
    } else {
        (crate::packed::pack_macro_zone_full(0, surface, 0, 0), cards::Micro::snap(0, 0))
    };

    let card_id = cards::next_card_id(ctx);
    cards::create_at(
        ctx,
        card_id,
        now_ms,
        macro_zone,
        micro,
        owner_id,
        packed_definition,
        /* flags */ 0,
        stock,
    );
    // Souls carry a private state row (blueprints); plain cards don't.
    if is_soul_card(packed_definition) {
        ctx.db.soul_privates().insert(SoulPrivate {
            card_id,
            blueprints_0: 0,
            active_blueprints: 0,
        });
    }
    Ok(())
}

