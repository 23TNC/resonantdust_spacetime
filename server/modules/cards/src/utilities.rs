use spacetimedb::{reducer, ReducerContext, Table};

use crate::cards;
use crate::flags::state_flags;
use crate::souls::{soul_privates as _soul_privates_table, with_portrait, SoulPrivate};

/// Surface the player's `player_soul` lives on. Deliberately `0` (not
/// the world band `64`) so the player-soul is never rendered anywhere
/// — it *is* the player, a thin soul that owns the world-facing souls
/// in its inventory rather than standing on the map itself.
const PLAYER_SOUL_SURFACE: u8 = 0;

/// Max player-souls a player may hold. The starter spawn is gated on this so a
/// re-login (the client re-requests a soul whenever its subscription hasn't yet
/// surfaced the existing one) can't mint duplicates. `1` today; bump for
/// multi-character. The whole spawn (player_soul + world soul + loadout) is
/// gated as a unit, since only the player_soul is owned by `player_id` directly.
const MAX_PLAYER_SOULS: usize = 1;

/// Spawn a new `player_soul` (and dev seed) owned by `player_id` in
/// this card shard, and return its `card_id`.
///
/// Called after the player has claimed/logged-in against the separate
/// `players` auth database (which hands back the `data_shard`) and the
/// client has connected here and found it owns no soul yet
/// (`cards.owner_id().filter(player_id)` empty). The created
/// `player_soul` carries `owner_id == player_id` + the
/// `is_owned_by_player` flag, so that same query then surfaces it.
///
/// **Idempotent per player up to [`MAX_PLAYER_SOULS`]** — the spawn is gated on
/// the player's current player-soul count, so a re-login (the client re-requests
/// whenever its subscription hasn't yet surfaced the existing soul) is a no-op
/// rather than minting a duplicate. Bump the cap for multi-character. These
/// top-level souls are never rendered in the world.
///
/// **Authorization is the gateway's job.** This reducer trusts its
/// `player_id` argument; a real deployment routes the call through the
/// gateway, which has the cross-DB view to verify the caller's
/// identity maps to `player_id` in the auth DB. Dev clients call it
/// directly.
///
/// The soul card write triggers `souls::on_card_write`, which
/// auto-creates the matching `Soul` row — so this fn never touches the
/// `Soul` table directly.
#[reducer]
pub fn spawn_soul(
    ctx: &ReducerContext,
    // Named `client_time_ms` (not `_client_time_ms`) because SpacetimeDB's HTTP
    // `/call` keys args on the exact Rust param name — the gateway/client send
    // `client_time_ms`, so an underscore here makes the call unaddressable.
    client_time_ms: u64,
    player_id: u32,
    // The client passes the soul index it expects to mint; accepted to match
    // the client's call shape (it gates "only if none" client-side). Unused
    // here — the dev seed below mints a fixed player_soul + world soul.
    soul_index: u32,
    // Gate-supplied packed defs (the gate owns content now — plan
    // `01_gate_authority_pivot`). `soul_packed` = the player_soul, `human_packed`
    // = the dev world-soul, `loadout_packed` = the dev loadout in spawn order.
    // The gate composes the loadout, so the list lives in one place.
    soul_packed: u16,
    human_packed: u16,
    loadout_packed: Vec<u16>,
) -> Result<(), String> {
    let _ = soul_index; // accepted to match the client's call shape; unused

    // Idempotency gate: the client re-requests a soul on every login (it can't
    // see the existing one until its subscription applies), so the SERVER caps
    // creation. If the player already holds the max player-souls, this is a
    // no-op `Ok` — the existing soul surfaces once the client's subscription
    // catches up. Reducers are DB-serialized, so even a rapid double-call is
    // safe: the first commits the soul, the second sees the count and skips.
    if cards::count_player_souls(ctx, player_id) >= MAX_PLAYER_SOULS {
        return Ok(());
    }

    // Stamp at the client's buffered clock — `effective_now_ms` = `min(client,
    // server)` within drift grace — so the soul lands on the client's promote
    // timeline and surfaces on the very next tick. This mirrors `request_zone`.
    //
    // The old fixed `now − TIME_DRIFT_BUFFER_MS` (2s) back-stamp was shallower
    // than the client's adaptive render buffer (`clientDelay`, 3–5s), so souls
    // spawned ~1–3s in the client's future. Unlike the bootstrap `claim_or_login`
    // (whose clock window is still empty), by the time the client calls
    // `spawn_soul` the login row has already seeded its offset window, so
    // `client_time_ms` is trustworthy here — a too-skewed value just errors and
    // the client retries.
    let time_ms = cards::effective_now_ms(ctx, client_time_ms)?;

    let soul_card_id = cards::next_card_id(ctx);

    let soul_def = soul_packed;

    // Deterministic 4-bit portrait pick — mixing the soul's card id
    // with `time_ms` and `player_id` gives a stable per-soul value
    // without an rng (reducers must stay deterministic).
    let portrait_seed = (time_ms as u32) ^ (time_ms >> 32) as u32 ^ player_id ^ soul_card_id;
    let portrait_id = ((portrait_seed ^ (portrait_seed >> 4)) & 0xF) as u8;
    let soul_flags_state = with_portrait(state_flags().is_owned_by_player, portrait_id);

    cards::create_at(
        ctx,
        soul_card_id,
        time_ms,
        /* macro_zone      */
        crate::packed::with_surface(crate::packed::pack_macro_zone(0, 0), PLAYER_SOUL_SURFACE),
        /* micro           */
        cards::Micro::snap(0, 0, crate::packed::loose_kind_for_surface(PLAYER_SOUL_SURFACE)),
        /* owner_id        */ player_id,
        soul_def,
        /* flags_state     */ soul_flags_state,
        /* flags_bk        */ 0,
    );

    // Empty per-soul private state — no starter blueprints granted.
    ctx.db.soul_privates().insert(SoulPrivate {
        card_id: soul_card_id,
        blueprints_0: 0,
        active_blueprints: 0,
    });

    // --- DEV TEST SEED: a world-facing "human" soul + a starter loadout in its
    // inventory. Ownership chain: player -> player_soul -> human -> {items}. The
    // human stands on the world at hex (0, 0); its inventory (surface =
    // INVENTORY_LAYER, owner = human card_id) holds a dust, three corpus, and an
    // axe. Disposable pre-release seeding — remove once real soul/inventory
    // acquisition exists.
    let human_def = human_packed;
    let human_card_id = cards::next_card_id(ctx);
    let human_portrait_seed =
        (time_ms as u32) ^ (time_ms >> 32) as u32 ^ player_id ^ human_card_id;
    let human_portrait_id = ((human_portrait_seed ^ (human_portrait_seed >> 4)) & 0xF) as u8;
    // Portrait only — NO `is_owned_by_player`. The human's `owner_id` is
    // the player_soul *card* (below), not a player_id; the flag means
    // exactly "owner_id is a player_id", so setting it here would make
    // owner-walks (`owning_player`) and soul counts mis-resolve the human
    // as a directly player-owned soul.
    let human_flags_state = with_portrait(0, human_portrait_id);
    cards::create_at(
        ctx,
        human_card_id,
        time_ms,
        /* macro_zone      */
        crate::packed::pack_macro_zone_full(0, crate::packed::WORLD_LAYER, 0, 0),
        /* micro           */
        cards::Micro::snap(0, 0, crate::packed::loose_kind_for_surface(crate::packed::WORLD_LAYER)),
        /* owner_id        */ soul_card_id,
        human_def,
        /* flags_state     */ human_flags_state,
        /* flags_bk        */ 0,
    );
    ctx.db.soul_privates().insert(SoulPrivate {
        card_id: human_card_id,
        blueprints_0: 0,
        active_blueprints: 0,
    });

    // Starter loadout in the human's inventory: a dust, three corpus, and an
    // axe. All land loose at cell (0, 0) of the inventory bucket; the client
    // lays them out across the grid (position is client-local).
    let inv_macro =
        crate::packed::pack_macro_zone_full(human_card_id, crate::packed::INVENTORY_LAYER, 0, 0);
    let inv_kind = crate::packed::loose_kind_for_surface(crate::packed::INVENTORY_LAYER);
    for &def in &loadout_packed {
        let card_id = cards::next_card_id(ctx);
        cards::create_at(
            ctx,
            card_id,
            time_ms,
            inv_macro,
            cards::Micro::snap(0, 0, inv_kind),
            /* owner_id    */ human_card_id,
            def,
            /* flags_state */ 0,
            /* flags_bk    */ 0,
        );
    }

    Ok(())
}

/// Add a single card to a specific soul's inventory bucket.
///
/// Dev/admin tool — invoked from the CLI, not the client. The
/// production client uses `propose_action` and never calls this. No
/// caller-identity check: the CLI sends an anonymous identity that
/// doesn't resolve to a player. The acting player for on-create
/// downstream is derived from `soul_card_id`'s `owning_player`
/// instead.
///
/// `card_key` is the bare card name from the DSL (e.g. `"attack"`,
/// `"fatigue"`). The gate resolves it to a `packed_definition` via the Bundle
/// (`bundle.packed_def(key)`) before this reducer is called.
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
    // Gate-supplied packed def (the gate resolves the card name from its Bundle).
    packed_definition: u16,
) -> Result<(), String> {
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;

    // Require the soul to exist and be player-owned (not world-owned).
    cards::owning_player(ctx, soul_card_id).ok_or_else(|| {
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

    Ok(())
}

