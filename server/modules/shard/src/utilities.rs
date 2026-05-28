use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, ReducerContext};

use crate::cards;
use crate::world_gen;

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
        /* micro_zone      */ 0,
        /* micro_location  */ 0,
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

/// Seed the world's terrain.
///
/// Delegates to [`world_gen::generate_forest_terrain`] against
/// `world_gen::WORLD_SEED` over a `BOOTSTRAP_WORLD_RADIUS` hex disk
/// around macro `(0, 0)`. Idempotent on re-runs: zone-tile bytes are
/// deterministic (so the second call regenerates identical rows),
/// and the world-card spawn path skips tiles already holding a world
/// card.
///
/// **No per-player setup happens here anymore.** A player's soul +
/// starter cards are spawned by `players::spawn_soul_for` on signup.
/// This reducer is purely a world-seeding entry point (admin / dev
/// tooling).
///
/// **Surface convention:** zone rows use `surface = 64` (first world
/// layer; the `< 64` range is reserved for inventory-ish surfaces,
/// see the q=1 force rule discussion in `actions.rs`).
#[reducer]
pub fn bootstrap(ctx: &ReducerContext) -> Result<(), String> {
    world_gen::generate_forest_terrain(ctx, world_gen::WORLD_SEED, BOOTSTRAP_WORLD_RADIUS)?;
    Ok(())
}

