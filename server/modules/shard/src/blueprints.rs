//! Blueprint-request reducer.
//!
//! Players "request" a blueprint by clicking a known blueprint in
//! the wrench panel and choosing a placement target. The reducer
//! spawns a real
//! `blueprint`-card-type row at the requested address, owned by
//! the acting soul, and counts it against the soul's
//! `active_blueprints` cap (sourced from the soul's
//! `aspects.builder` value).
//!
//! Auth: the caller's identity must resolve to the player who
//! owns the requesting soul. Without this gate a malicious client
//! could spawn blueprints at an arbitrary address or under
//! another player's soul.
//!
//! Resolution of placed blueprints (commit / cancel — building the
//! actual structure, returning the slot to the cap) is TBD; this
//! reducer only owns the SPAWN side.

use resonantdust_content::blueprint_core::{blueprint, BLUEPRINT_NONE};
use resonantdust_content::definition_core::{
    aspect_id as core_aspect_id, decode_definition, is_aspect_descendant,
};
use spacetimedb::{reducer, ReducerContext};

use crate::cards;
use crate::flags::state_flags;
use crate::players;
use crate::souls::{is_soul_card, soul_privates as _soul_privates_table};

/// Place a blueprint card at the requested location, owned by the
/// caller's soul.
///
/// **Steps:**
/// 1. Caller identity → `player_id`. `now_ms` via `effective_now_ms`.
/// 2. Validate the soul:
///    - row exists at `now_ms`, not dead
///    - is a `soul`-type card
///    - no in-flight holds (slot / share / position)
///    - `owning_player == caller_player_id` (anti-spoof gate —
///      without this a client could pass any soul_card_id and
///      spawn blueprints owned by other players' souls)
/// 3. Validate the blueprint:
///    - `blueprint_id` is non-zero and registered in the catalog
///    - soul has the discovery bit set in `SoulPrivate.blueprints_0`
/// 4. Validate the cap:
///    - read the soul def's `aspects.builder` (sum descendants —
///      `crafting` → `builder` widening) as `max_active`
///    - reject if `active_blueprints >= max_active`
/// 5. Spawn the blueprint card at `(surface, macro_zone, micro_location
///    micro_location)` with `owner_id = soul_card_id`. Bump
///    `SoulPrivate.active_blueprints`.
///
/// **Idempotency.** No dedup gate today — repeated calls spawn
/// repeated cards (subject to the cap). The client should dedupe
/// by suppressing the next request until the prior one's spawn
/// row arrives via subscription.
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn request_blueprint(
    ctx: &ReducerContext,
    client_time_ms: u64,
    soul_card_id: u32,
    blueprint_id: u16,
    surface: u8,
    macro_zone: u64,
    micro_location: u32,
) -> Result<(), String> {
    // ---- caller + soul resolution --------------------------------
    let caller_player_id = players::resolve_caller(ctx)?;
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;

    let soul = cards::prior_at(ctx, soul_card_id, now_ms)
        .ok_or_else(|| format!("request_blueprint: soul card {soul_card_id} not found"))?;
    let s = state_flags();
    if soul.flags_state & s.dead != 0 {
        return Err(format!(
            "request_blueprint: soul card {soul_card_id} is dead"
        ));
    }
    if !is_soul_card(soul.packed_definition) {
        return Err(format!(
            "request_blueprint: card {soul_card_id} is not a soul-type card"
        ));
    }
    if cards::slot_hold_count(soul.flags_bk) > 0 {
        return Err(format!(
            "request_blueprint: soul {soul_card_id} is exclusively held by an in-flight action"
        ));
    }
    if cards::slot_share_count(soul.flags_bk) > 0 {
        return Err(format!(
            "request_blueprint: soul {soul_card_id} is shared-held by an in-flight action"
        ));
    }
    if cards::position_hold_count(soul.flags_bk) > 0 {
        return Err(format!(
            "request_blueprint: soul {soul_card_id} is position-held by an in-flight action"
        ));
    }
    // Identity gate — caller's player must own the soul. Souls
    // carry `FLAG_OWNED_BY_PLAYER`, so `owning_player` walks one
    // hop. World-owned cards (chain hits 0) resolve to
    // `WORLD_PLAYER_ID`; treat that the same as "not yours".
    let soul_player = cards::owning_player(ctx, soul_card_id)
        .unwrap_or(cards::WORLD_PLAYER_ID);
    if soul_player != caller_player_id {
        return Err(format!(
            "request_blueprint: soul {soul_card_id} owned by player {soul_player} \
             (not {caller_player_id})"
        ));
    }

    // ---- blueprint catalog + access lookups ----------------------
    if blueprint_id == BLUEPRINT_NONE {
        return Err("request_blueprint: blueprint_id 0 is reserved".to_string());
    }
    let bp = blueprint(blueprint_id)
        .map_err(|e| format!("request_blueprint: blueprint registry: {e}"))?
        .ok_or_else(|| format!("request_blueprint: blueprint id {blueprint_id} not registered"))?;

    let soul_private = ctx
        .db
        .soul_privates()
        .card_id()
        .find(soul_card_id)
        .ok_or_else(|| {
            format!("request_blueprint: no SoulPrivate row for soul {soul_card_id}")
        })?;
    // `blueprints_0` covers ids 1..=64 (bit position = id - 1).
    // Anything past that bucket lives in a future `blueprints_1`
    // we haven't added yet — reject so callers know the discovery
    // can't be expressed in storage rather than silently denying.
    if blueprint_id > 64 {
        return Err(format!(
            "request_blueprint: blueprint id {blueprint_id} outside the blueprints_0 bucket \
             (1..=64); extend SoulPrivate before requesting"
        ));
    }
    let bit = 1u64 << (blueprint_id - 1);
    if soul_private.blueprints_0 & bit == 0 {
        return Err(format!(
            "request_blueprint: soul {soul_card_id} has not discovered blueprint {blueprint_id} \
             (key={:?})",
            bp.key
        ));
    }

    // ---- cap check: builder aspect vs live blueprint count -------
    let builder_aspect_id = core_aspect_id("builder")
        .map_err(|e| format!("request_blueprint: aspect lookup: {e}"))?
        .ok_or_else(|| {
            "request_blueprint: \"builder\" aspect not registered in content catalog".to_string()
        })?;
    let soul_def = decode_definition(soul.packed_definition)
        .map_err(|e| format!("request_blueprint: decode soul def: {e}"))?
        .ok_or_else(|| format!("request_blueprint: soul def unknown for {soul_card_id}"))?;
    // Sub-aspect widening: a soul carrying `aspects.builder = 1`
    // matches; so would a hypothetical `aspects.crafting = 2`
    // (descendant counts toward parent). Negative values shouldn't
    // happen in content but clamp to 0 to be safe.
    let max_active: i32 = soul_def
        .aspects
        .iter()
        .filter(|(id, _)| is_aspect_descendant(*id, builder_aspect_id).unwrap_or(false))
        .map(|(_, v)| *v as i32)
        .sum::<i32>()
        .max(0);
    if max_active <= 0 {
        return Err(format!(
            "request_blueprint: soul {soul_card_id} has no `builder` aspect (max_active=0)"
        ));
    }
    if (soul_private.active_blueprints as i32) >= max_active {
        return Err(format!(
            "request_blueprint: soul {soul_card_id} already has {} active blueprints \
             (cap {max_active} from builder aspect)",
            soul_private.active_blueprints
        ));
    }

    // ---- spawn the blueprint card --------------------------------
    //
    // No bookkeeping write needed in this reducer — the
    // `on_card_write` hook in `souls.rs` recognises the spawned
    // blueprint, walks its owner chain to this soul, and bumps
    // `SoulPrivate.active_blueprints` itself. The same hook
    // decrements when the blueprint goes `FLAG_DEAD` or its owner
    // changes, so no slot-release reducer is needed.
    let card_id = cards::next_card_id(ctx);
    // The blueprint lands loose at the requested cell/offset on `surface`.
    let (lq, lr, x, y) = crate::packed::unpack_micro_loose(micro_location);
    let micro = cards::Micro::Loose {
        local_q: lq,
        local_r: lr,
        x,
        y,
        kind: crate::packed::loose_kind_for_surface(surface),
    };
    cards::create_at(
        ctx,
        card_id,
        now_ms,
        crate::packed::with_surface(macro_zone, surface),
        micro,
        /* owner_id */ soul_card_id,
        bp.blueprint_packed_definition,
        /* flags_state */ 0,
        /* flags_bk */ 0,
    );

    Ok(())
}
