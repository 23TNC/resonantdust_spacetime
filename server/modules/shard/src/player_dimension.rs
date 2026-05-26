//! Player-pocket-dimension reducers.
//!
//! A player dimension is a private 2×2 grid of `Zone`s carved out of
//! `PLAYER_DIMENSION_LAYER (62)`. Address shape:
//!
//!     (surface = 62, macro_zone = pack(chunk_q, chunk_r), owner_id = player_id)
//!
//! Multiple players' dimensions coexist at the same `macro_zone` —
//! `Zone.owner_id` discriminates. The dimension's chunks live at
//! `(chunk_q, chunk_r) ∈ {0, 1} × {0, 1}` (coords independent of the
//! overworld; each player's dim starts at origin).
//!
//! Soul cards "visit" by being rewritten to `(surface=62,
//! macro_zone=pack(q,r), micro_zone=pack(lq,lr,Free))`. Their
//! `owner_id` stays at `player_id` (souls always carry
//! `FLAG_OWNED_BY_PLAYER`), which is also the dim's discriminator —
//! so soul-side reads filter to the right dim automatically.
//!
//! Inventory follows the soul implicitly: inventory cards are keyed
//! by `macro_zone = soul.card_id`, not by the soul's physical
//! location, so a visiting soul's bag travels with it.
//!
//! V1 scope:
//! - `enter_player_dimension` moves a caller-owned soul from
//!   wherever it currently sits onto a tile of that caller's dim.
//! - `exit_player_dimension` does the inverse — moves the soul back
//!   to a specified world hex.
//! - Cross-soul interactions inside the dim follow the existing
//!   soul-scoped ownership gate (see `docs/OWNERSHIP_MODEL.md`).
//!   Same-player souls in the dim don't get a relaxed gate.

use spacetimedb::{reducer, ReducerContext};

use crate::cards::{self, cards as _cards_table};
use crate::flags::state_flags;
use crate::packed::{
    pack_macro_zone, pack_micro_zone, unpack_micro_zone, StackedState, PLAYER_DIMENSION_LAYER,
    WORLD_LAYER,
};
use crate::players;
use crate::souls::is_soul_card;
use crate::zones;

/// Move a caller-owned soul card into that caller's player
/// dimension at `(chunk_q, chunk_r, local_q, local_r)`.
///
/// **Steps:**
/// 1. Resolve caller → `player_id`. Resolve `now_ms`.
/// 2. Lifecycle-magnetic block gate (no carve-out; teleporting a
///    soul into the dim is an explicit player choice and shouldn't
///    bypass pending magnetic resolution).
/// 3. Validate the soul: exists, alive, is `soul`-type, no
///    in-flight holds, `owning_player == caller`.
/// 4. Validate the target coords: chunks within the 2×2 grid,
///    locals within the 8×8 per-chunk grid, target Zone exists
///    (sanity — created eagerly at `claim_or_login`).
/// 5. Rewrite the soul row: `surface = PLAYER_DIMENSION_LAYER`,
///    `macro_zone = pack(chunk_q, chunk_r)`,
///    `micro_zone = pack(local_q, local_r, Free)`,
///    `micro_location = 0`. `owner_id` is left alone — souls
///    already carry `owner_id = player_id`.
///
/// **Idempotent re-entry.** Re-calling with the same target on a
/// soul already in the dim writes a fresh row stamped at `now_ms`
/// but with no positional change; harmless and the row is GC'd
/// like any other history entry.
#[reducer]
pub fn enter_player_dimension(
    ctx: &ReducerContext,
    client_time_ms: u64,
    soul_card_id: u32,
    chunk_q: i16,
    chunk_r: i16,
    local_q: u8,
    local_r: u8,
) -> Result<(), String> {
    let caller_player_id = players::resolve_caller(ctx)?;
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;
    crate::lifecycle_pending::block_check(ctx, caller_player_id, now_ms, &[])?;

    validate_soul_for_move(ctx, soul_card_id, caller_player_id, now_ms, "enter_player_dimension")?;
    validate_target_coords(chunk_q, chunk_r, local_q, local_r, "enter_player_dimension")?;

    // Sanity check — the dim's Zone for this chunk must exist (it's
    // created eagerly in `claim_or_login`). Mismatch here means
    // either a content/seed bug or an old player from before the
    // dim feature; bail clearly rather than land the soul on
    // unmapped surface.
    let macro_zone = pack_macro_zone(chunk_q, chunk_r);
    if zones::latest_for_owner(ctx, PLAYER_DIMENSION_LAYER, macro_zone, caller_player_id).is_none()
    {
        return Err(format!(
            "enter_player_dimension: no dim Zone at (chunk_q={chunk_q}, chunk_r={chunk_r}) \
             for player {caller_player_id} — was the player created before the dim feature?"
        ));
    }

    let micro_zone = pack_micro_zone(local_q, local_r, StackedState::Free);
    cards::update_with_at(ctx, soul_card_id, now_ms, |c| {
        c.surface = PLAYER_DIMENSION_LAYER;
        c.macro_zone = macro_zone;
        c.micro_zone = micro_zone;
        c.micro_location = 0;
    });

    Ok(())
}

/// Inverse of `enter_player_dimension`: move a soul currently in
/// the caller's dim back to a world hex.
///
/// **Steps:** as enter, but:
/// - Soul must currently be on `PLAYER_DIMENSION_LAYER` (otherwise
///   "exit from where?" is meaningless).
/// - Target `(world_macro_zone, world_micro_zone)` validated as a
///   Free world position with a Zone backing it and no live
///   occupant — mirrors `deploy_mini_zone`'s landing-spot rules.
///
/// The caller supplies the world destination explicitly; the server
/// doesn't track "last world position" anywhere.
#[reducer]
pub fn exit_player_dimension(
    ctx: &ReducerContext,
    client_time_ms: u64,
    soul_card_id: u32,
    world_macro_zone: u32,
    world_micro_zone: u8,
) -> Result<(), String> {
    let caller_player_id = players::resolve_caller(ctx)?;
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;
    crate::lifecycle_pending::block_check(ctx, caller_player_id, now_ms, &[])?;

    let soul = validate_soul_for_move(
        ctx,
        soul_card_id,
        caller_player_id,
        now_ms,
        "exit_player_dimension",
    )?;
    if soul.surface != PLAYER_DIMENSION_LAYER {
        return Err(format!(
            "exit_player_dimension: soul {soul_card_id} is not in a player dimension \
             (current surface={})",
            soul.surface
        ));
    }

    // ---- target validation ---------------------------------------
    let (_t_q, _t_r, t_state) = unpack_micro_zone(world_micro_zone);
    if t_state != StackedState::Free {
        return Err(format!(
            "exit_player_dimension: target micro_zone must be Free; got {t_state:?}"
        ));
    }
    if zones::latest_for(ctx, WORLD_LAYER, world_macro_zone).is_none() {
        return Err(format!(
            "exit_player_dimension: no world zone at macro_zone={world_macro_zone}; \
             cannot exit into unmapped area"
        ));
    }
    let s = state_flags();
    let occupied = ctx
        .db
        .cards()
        .macro_zone()
        .filter(world_macro_zone)
        .any(|c| {
            if c.surface != WORLD_LAYER {
                return false;
            }
            if c.flags_state & s.dead != 0 {
                return false;
            }
            let Some(latest) = cards::prior_at(ctx, c.card_id, now_ms) else {
                return false;
            };
            latest.surface == WORLD_LAYER
                && latest.macro_zone == world_macro_zone
                && latest.micro_zone == world_micro_zone
                && latest.flags_state & s.dead == 0
        });
    if occupied {
        return Err(format!(
            "exit_player_dimension: target world hex (macro_zone={world_macro_zone}, \
             micro_zone={world_micro_zone}) already has an occupant"
        ));
    }

    cards::update_with_at(ctx, soul_card_id, now_ms, |c| {
        c.surface = WORLD_LAYER;
        c.macro_zone = world_macro_zone;
        c.micro_zone = world_micro_zone;
        c.micro_location = 0;
    });

    Ok(())
}

// ---- shared validators -----------------------------------------------

/// Shared soul-validation used by both reducers: card exists,
/// alive, soul-type, no in-flight holds, owned by `caller_player_id`.
/// Returns the resolved soul row for follow-up location checks.
fn validate_soul_for_move(
    ctx: &ReducerContext,
    soul_card_id: u32,
    caller_player_id: u32,
    now_ms: u64,
    reducer_name: &str,
) -> Result<cards::Card, String> {
    let soul = cards::prior_at(ctx, soul_card_id, now_ms)
        .ok_or_else(|| format!("{reducer_name}: soul card {soul_card_id} not found"))?;
    let s = state_flags();
    if soul.flags_state & s.dead != 0 {
        return Err(format!(
            "{reducer_name}: soul card {soul_card_id} is dead"
        ));
    }
    if !is_soul_card(soul.packed_definition) {
        return Err(format!(
            "{reducer_name}: card {soul_card_id} is not a soul-type card"
        ));
    }
    if cards::slot_hold_count(soul.flags_bk) > 0 {
        return Err(format!(
            "{reducer_name}: soul {soul_card_id} is exclusively held by an in-flight action"
        ));
    }
    if cards::slot_share_count(soul.flags_bk) > 0 {
        return Err(format!(
            "{reducer_name}: soul {soul_card_id} is shared-held by an in-flight action"
        ));
    }
    if cards::position_hold_count(soul.flags_bk) > 0 {
        return Err(format!(
            "{reducer_name}: soul {soul_card_id} is position-held by an in-flight action"
        ));
    }
    // Souls carry `FLAG_OWNED_BY_PLAYER` and have `owner_id =
    // player_id` directly. `owning_player` is robust either way
    // (walks one step on a flagged row).
    let soul_player = cards::owning_player(ctx, soul_card_id)
        .unwrap_or(cards::WORLD_PLAYER_ID);
    if soul_player != caller_player_id {
        return Err(format!(
            "{reducer_name}: soul {soul_card_id} is owned by player {soul_player} \
             (not {caller_player_id})"
        ));
    }
    Ok(soul)
}

/// Range-check target coords for `enter_player_dimension`. Chunks
/// must fall inside the 2×2 grid; locals inside the per-chunk 8×8.
fn validate_target_coords(
    chunk_q: i16,
    chunk_r: i16,
    local_q: u8,
    local_r: u8,
    reducer_name: &str,
) -> Result<(), String> {
    let grid = zones::PLAYER_DIMENSION_GRID_SIZE;
    if chunk_q < 0 || chunk_q >= grid || chunk_r < 0 || chunk_r >= grid {
        return Err(format!(
            "{reducer_name}: chunk_q/chunk_r out of range — got ({chunk_q}, {chunk_r}), \
             grid is {grid}×{grid}"
        ));
    }
    if local_q >= 8 || local_r >= 8 {
        return Err(format!(
            "{reducer_name}: local_q/local_r must be 0..8 — got ({local_q}, {local_r})"
        ));
    }
    Ok(())
}
