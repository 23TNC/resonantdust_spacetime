//! Gate-facing write reducers — the surface the gateway calls to apply a
//! validated recipe's effects to this `cards` shard. Authorization is the
//! gateway's job: these trust their arguments (same posture as `spawn_soul`).
//!
//! Covers the dedup gate, hold acquire/release, create/destroy, and the
//! chain-stitch reposition primitives (`move_card` loose / `stack_card`
//! member) the gateway's apply step calls to reproduce `propose_action`.

use resonantdust_content::definition_core::find_packed_by_key;
use spacetimedb::{reducer, ReducerContext, Table};

use crate::cards;
use crate::cards::MicroPlace;
use crate::flags::state_flags;
use crate::packed::{loose_kind_for_surface, unpack_micro_loose, with_surface};
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

/// Reposition `card_id` **loose** at the proposed cell on `surface` within
/// `macro_zone`, stamped at `time_ms`, asserting `pos_need`. This is the
/// chain-stitch ROOT placement — the recipe root lands loose at the address the
/// client proposed. `micro_location` is the packed loose cell `(q, r, x, y)`.
/// Mirrors `shard::actions::chain_stitch`'s root arm.
#[reducer]
pub fn move_card(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    surface: u8,
    macro_zone: u64,
    micro_location: u32,
) -> Result<(), String> {
    let pos_need = state_flags().pos_need;
    let full_macro = with_surface(macro_zone, surface);
    let (q, r, x, y) = unpack_micro_loose(micro_location);
    let micro = cards::Micro::Loose {
        local_q: q,
        local_r: r,
        x,
        y,
        kind: loose_kind_for_surface(surface),
    };
    if cards::update_with_at(ctx, card_id, time_ms, |c| {
        c.macro_zone = full_macro;
        micro.place(c);
        c.flags_state |= pos_need;
    })
    .is_none()
    {
        return Err(format!("move_card: card {card_id} not found"));
    }
    Ok(())
}

/// Reposition `card_id` as a flat **member** of `root` in `branch` at `index`,
/// sharing `root`'s `(surface, macro_zone)`, stamped at `time_ms`, asserting
/// `pos_need`. This is the chain-stitch MEMBER placement — top-level iterator
/// bindings become flat members of the recipe root. Mirrors
/// `shard::actions::chain_stitch`'s member arm.
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn stack_card(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    surface: u8,
    macro_zone: u64,
    root: u32,
    branch: u8,
    index: u8,
) -> Result<(), String> {
    let pos_need = state_flags().pos_need;
    let full_macro = with_surface(macro_zone, surface);
    let micro = cards::Micro::Stacked {
        root,
        branch,
        index: index.min(15),
    };
    if cards::update_with_at(ctx, card_id, time_ms, |c| {
        c.macro_zone = full_macro;
        micro.place(c);
        c.flags_state |= pos_need;
    })
    .is_none()
    {
        return Err(format!("stack_card: card {card_id} not found"));
    }
    Ok(())
}

/// Finalize a bound card at action completion: clear the position-assertion
/// bits (`pos_need`/`pos_want`) and stamp `progress_style` (bits 5-7 of
/// `flags_state`) so the client renders the action's progress bar on this card's
/// completion row. This is the `flags_state` half of the monolith's per-card
/// `action_completion::commit` write; the gate calls it for every bound card
/// (`progress_style = 0` clears the bar, so non-actor cards don't render a stale
/// one). Composes with `destroy_card` / `release_hold` at the same `time_ms`.
#[reducer]
pub fn finalize_card(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    progress_style: u8,
) -> Result<(), String> {
    let s = state_flags();
    let style_bits = ((progress_style as u32) << s.progress_style_shift) & s.progress_style_mask;
    if cards::update_with_at(ctx, card_id, time_ms, |c| {
        c.flags_state &= !s.pos_need;
        c.flags_state &= !s.pos_want;
        c.flags_state = (c.flags_state & !s.progress_style_mask) | style_bits;
    })
    .is_none()
    {
        return Err(format!("finalize_card: card {card_id} not found"));
    }
    Ok(())
}

/// Set a blueprint's discovery bit on `target_card_id`'s `SoulPrivate`
/// (`<soul>.blueprint.unlock: <key>`). Idempotent — re-firing on an already-set
/// bit is a no-op. Port of the monolith's `apply_unlock_blueprint`.
#[reducer]
pub fn unlock_blueprint(
    ctx: &ReducerContext,
    target_card_id: u32,
    blueprint_key: String,
) -> Result<(), String> {
    use crate::souls::soul_privates as _soul_privates_table;
    let bp = resonantdust_content::blueprint_core::find_blueprint(&blueprint_key)
        .map_err(|e| format!("unlock_blueprint: catalog lookup: {e}"))?
        .ok_or_else(|| format!("unlock_blueprint: blueprint {blueprint_key:?} not registered"))?;
    if bp.id == 0 || bp.id > 64 {
        return Err(format!(
            "unlock_blueprint: blueprint id {} outside the blueprints_0 bucket (1..=64)",
            bp.id
        ));
    }
    let bit = 1u64 << (bp.id - 1);
    let Some(mut row) = ctx.db.soul_privates().card_id().find(target_card_id) else {
        return Err(format!(
            "unlock_blueprint: no SoulPrivate row for target card {target_card_id}"
        ));
    };
    if row.blueprints_0 & bit != 0 {
        return Ok(());
    }
    row.blueprints_0 |= bit;
    ctx.db.soul_privates().card_id().delete(target_card_id);
    ctx.db.soul_privates().insert(row);
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
