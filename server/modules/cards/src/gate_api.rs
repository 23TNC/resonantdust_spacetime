//! Gate-facing write reducers — the surface the gateway calls to apply a
//! validated recipe's effects to this `cards` shard. Authorization is the
//! gateway's job: these trust their arguments (same posture as `spawn_soul`).
//!
//! Card create/move land here once the apply/plan port (W7) pins their exact
//! positioning semantics; this first cut covers the unambiguous pieces — the
//! dedup gate, hold acquire/release, and destroy.

use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, ReducerContext};

use crate::cards;
use crate::flags::state_flags;
use crate::packed::{loose_kind_for_surface, with_surface};
use crate::pending_actions;

/// Hold-family selector shared with the gateway. Keep in sync with the
/// gateway's apply step.
mod hold_kind {
    pub const TOUCH: u8 = 0;
    pub const SLOT_HOLD: u8 = 1;
    pub const SLOT_SHARE: u8 = 2;
    pub const POSITION_HOLD: u8 = 3;
    pub const DROP_HOLD: u8 = 4;
    pub const SERVER: u8 = 5;
}

/// Claim the in-flight slot for a `(recipe, root, bindings)` tuple. Rejects if
/// the same tuple is already in flight — the DB-side dedup the gateway relies
/// on. `completion_ms` is when the action's effects are stamped.
#[reducer]
pub fn claim_pending(
    ctx: &ReducerContext,
    recipe_id: u16,
    root: u32,
    bindings: Vec<Vec<u32>>,
    completion_ms: u64,
) -> Result<(), String> {
    let key = pending_actions::dedup_key(recipe_id, root, &bindings);
    if pending_actions::is_in_flight(ctx, key) {
        return Err(format!("action already in flight (dedup_key {key:#018x})"));
    }
    pending_actions::install(ctx, key, completion_ms);
    Ok(())
}

/// Release the in-flight slot for a tuple (no-op if absent). Called after the
/// gateway has written the action's completion-time effects.
#[reducer]
pub fn release_pending(
    ctx: &ReducerContext,
    recipe_id: u16,
    root: u32,
    bindings: Vec<Vec<u32>>,
) -> Result<(), String> {
    pending_actions::release(ctx, pending_actions::dedup_key(recipe_id, root, &bindings));
    Ok(())
}

/// Acquire one reference of hold `kind` on `card_id` at `time_ms`.
#[reducer]
pub fn acquire_hold(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    kind: u8,
) -> Result<(), String> {
    match kind {
        hold_kind::TOUCH => cards::acquire_touch(ctx, card_id, time_ms),
        hold_kind::SLOT_HOLD => cards::acquire_slot_hold(ctx, card_id, time_ms),
        hold_kind::SLOT_SHARE => cards::acquire_slot_share(ctx, card_id, time_ms),
        hold_kind::POSITION_HOLD => cards::acquire_position_hold(ctx, card_id, time_ms),
        hold_kind::DROP_HOLD => cards::acquire_drop_hold(ctx, card_id, time_ms),
        hold_kind::SERVER => cards::acquire_server(ctx, card_id, time_ms),
        other => return Err(format!("acquire_hold: unknown hold kind {other}")),
    }
    Ok(())
}

/// Release one reference of hold `kind` on `card_id` at `time_ms`.
#[reducer]
pub fn release_hold(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    kind: u8,
) -> Result<(), String> {
    match kind {
        hold_kind::TOUCH => cards::release_touch(ctx, card_id, time_ms),
        hold_kind::SLOT_HOLD => cards::release_slot_hold(ctx, card_id, time_ms),
        hold_kind::SLOT_SHARE => cards::release_slot_share(ctx, card_id, time_ms),
        hold_kind::POSITION_HOLD => cards::release_position_hold(ctx, card_id, time_ms),
        hold_kind::DROP_HOLD => cards::release_drop_hold(ctx, card_id, time_ms),
        hold_kind::SERVER => cards::release_server(ctx, card_id, time_ms),
        other => return Err(format!("release_hold: unknown hold kind {other}")),
    }
    Ok(())
}

/// Create a card of `def_key`, loose at cell (0,0) on `surface` within
/// `macro_zone`, owned by `owner_id`, stamped at `time_ms`. The id is
/// allocated here (`next_card_id`, embedding this shard). Mirrors the legacy
/// `action_completion::commit` `Effect::Create` path; the gateway calls it for
/// a recipe's create outputs (now- or completion-stamped).
#[reducer]
pub fn create_card(
    ctx: &ReducerContext,
    time_ms: u64,
    def_key: String,
    surface: u8,
    macro_zone: u64,
    owner_id: u32,
) -> Result<(), String> {
    let packed_def = find_packed_by_key(&def_key)
        .map_err(|e| format!("create_card: find_packed_by_key({def_key:?}): {e}"))?
        .ok_or_else(|| format!("create_card: def {def_key:?} not registered in cards/id.json"))?;
    let new_id = cards::next_card_id(ctx);
    cards::create_at(
        ctx,
        new_id,
        time_ms,
        with_surface(macro_zone, surface),
        cards::Micro::snap(0, 0, loose_kind_for_surface(surface)),
        owner_id,
        packed_def,
        /* flags_state */ 0,
        /* flags_bk */ 0,
    );
    Ok(())
}

/// Destroy `card_id` at `time_ms` — stamp the `dead` state flag forward.
#[reducer]
pub fn destroy_card(ctx: &ReducerContext, card_id: u32, time_ms: u64) -> Result<(), String> {
    let dead = state_flags().dead;
    if cards::update_with_at(ctx, card_id, time_ms, |c| c.flags_state |= dead).is_none() {
        return Err(format!("destroy_card: card {card_id} not found"));
    }
    Ok(())
}
