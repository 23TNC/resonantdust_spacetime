use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, ReducerContext, Table};

use crate::cards;
use crate::flags::state_flags;
use crate::souls::{soul_privates as _soul_privates_table, with_portrait, SoulPrivate};

/// Surface the player's `player_soul` lives on. Deliberately `0` (not
/// the world band `64`) so the player-soul is never rendered anywhere
/// — it *is* the player, a thin soul that owns the world-facing souls
/// in its inventory rather than standing on the map itself.
const PLAYER_SOUL_SURFACE: u8 = 0;

/// Soul auto-granted to every player. Resolved through the content
/// catalog; a fresh account is always a `player_soul`.
const STARTER_SOUL_KEY: &str = "player_soul";

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
/// **Not idempotent — each call mints a fresh soul** with its own
/// `next_card_id`. A player may own several top-level player-souls;
/// that's the future multi-character handle. The "only if none exist"
/// gate lives client-side. These top-level souls are never rendered in
/// the world.
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
    _client_time_ms: u64,
    player_id: u32,
) -> Result<(), String> {
    // Stamp at `now − TIME_DRIFT_BUFFER_MS` so the rows are immediately
    // visible to the client's buffered `serverNowMs()` view (mirrors the
    // convention the auth DB's `claim_or_login` uses for the player row).
    let time_ms = cards::now_ms(ctx).saturating_sub(cards::TIME_DRIFT_BUFFER_MS);

    let soul_card_id = cards::next_card_id(ctx);

    let soul_def = find_packed_by_key(STARTER_SOUL_KEY)?.ok_or_else(|| {
        format!("spawn_soul: soul def {STARTER_SOUL_KEY:?} not in content catalog")
    })?;

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

    // --- DEV TEST SEED: a world-facing "human" soul + a dust in its inventory.
    // Ownership chain: player -> player_soul -> human -> dust. The human stands
    // on the world at hex (0, 0); its inventory (surface = INVENTORY_LAYER,
    // owner = human card_id) holds one dust. Disposable pre-release seeding —
    // remove once real soul/inventory acquisition exists.
    let human_def = find_packed_by_key("human")?
        .ok_or_else(|| "spawn_soul: \"human\" def not in content catalog".to_string())?;
    let human_card_id = cards::next_card_id(ctx);
    let human_portrait_seed =
        (time_ms as u32) ^ (time_ms >> 32) as u32 ^ player_id ^ human_card_id;
    let human_portrait_id = ((human_portrait_seed ^ (human_portrait_seed >> 4)) & 0xF) as u8;
    let human_flags_state = with_portrait(state_flags().is_owned_by_player, human_portrait_id);
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

    let dust_def = find_packed_by_key("dust")?
        .ok_or_else(|| "spawn_soul: \"dust\" def not in content catalog".to_string())?;
    let dust_card_id = cards::next_card_id(ctx);
    cards::create_at(
        ctx,
        dust_card_id,
        time_ms,
        /* macro_zone      */
        crate::packed::pack_macro_zone_full(human_card_id, crate::packed::INVENTORY_LAYER, 0, 0),
        /* micro           */
        cards::Micro::snap(0, 0, crate::packed::loose_kind_for_surface(crate::packed::INVENTORY_LAYER)),
        /* owner_id        */ human_card_id,
        dust_def,
        /* flags_state     */ 0,
        /* flags_bk        */ 0,
    );

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

