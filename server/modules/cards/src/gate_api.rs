//! Gate-facing write reducers — the surface the gateway calls to apply a
//! validated recipe's effects to this `cards` shard. Authorization is the
//! gateway's job: these trust their arguments (same posture as `spawn_soul`).
//!
//! Covers the dedup gate, hold acquire/release, create/destroy, and the
//! chain-stitch reposition primitives (`move_card` loose / `stack_card`
//! member) the gateway's apply step calls to reproduce `propose_action`.

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

/// Dispatch acquire (`acquire=true`) or release of hold `kind` on `card_id` at
/// `time_ms` to the matching refcount helper. Shared by the lease reducer and
/// the standalone acquire/release reducers.
fn dispatch_hold(
    ctx: &ReducerContext,
    card_id: u32,
    time_ms: u64,
    kind: u8,
    acquire: bool,
) -> Result<(), String> {
    match (kind, acquire) {
        (hold_kind::TOUCH, true) => cards::acquire_touch(ctx, card_id, time_ms),
        (hold_kind::TOUCH, false) => cards::release_touch(ctx, card_id, time_ms),
        (hold_kind::SLOT_HOLD, true) => cards::acquire_slot_hold(ctx, card_id, time_ms),
        (hold_kind::SLOT_HOLD, false) => cards::release_slot_hold(ctx, card_id, time_ms),
        (hold_kind::SLOT_SHARE, true) => cards::acquire_slot_share(ctx, card_id, time_ms),
        (hold_kind::SLOT_SHARE, false) => cards::release_slot_share(ctx, card_id, time_ms),
        (hold_kind::POSITION_HOLD, true) => cards::acquire_position_hold(ctx, card_id, time_ms),
        (hold_kind::POSITION_HOLD, false) => cards::release_position_hold(ctx, card_id, time_ms),
        (hold_kind::DROP_HOLD, true) => cards::acquire_drop_hold(ctx, card_id, time_ms),
        (hold_kind::DROP_HOLD, false) => cards::release_drop_hold(ctx, card_id, time_ms),
        (hold_kind::SERVER, true) => cards::acquire_server(ctx, card_id, time_ms),
        (hold_kind::SERVER, false) => cards::release_server(ctx, card_id, time_ms),
        (other, _) => return Err(format!("hold: unknown kind {other}")),
    }
    Ok(())
}

/// Acquire one reference of hold `kind` on `card_id` at `time_ms`.
#[reducer]
pub fn acquire_hold(ctx: &ReducerContext, card_id: u32, time_ms: u64, kind: u8) -> Result<(), String> {
    dispatch_hold(ctx, card_id, time_ms, kind, true)
}

/// Release one reference of hold `kind` on `card_id` at `time_ms`.
#[reducer]
pub fn release_hold(ctx: &ReducerContext, card_id: u32, time_ms: u64, kind: u8) -> Result<(), String> {
    dispatch_hold(ctx, card_id, time_ms, kind, false)
}

/// Acquire a **self-expiring lease** of slot hold `kind` on `card_id`: take the
/// hold at `acquire_ms` and write its matching release at `release_ms`, in one
/// transaction. The multi-gate concurrency guard — an atomic reader/writer
/// check-and-set on the row current at `acquire_ms`:
///   - `SLOT_HOLD` (exclusive `use`/`claim`): reject if any exclusive **or**
///     shared hold is live.
///   - `SLOT_SHARE` (`share`/`borrow`): reject only if exclusively held.
/// Whichever gate's reducer commits first wins; the loser is rejected and backs
/// out (its other leases self-expire). Because the release is written here, the
/// lock self-heals at `release_ms` even if the acquiring gate crashes.
#[reducer]
pub fn acquire_lease(
    ctx: &ReducerContext,
    card_id: u32,
    kind: u8,
    acquire_ms: u64,
    release_ms: u64,
) -> Result<(), String> {
    let bk = cards::prior_at(ctx, card_id, acquire_ms).map(|c| c.flags_bk).unwrap_or(0);
    let exclusive = cards::slot_hold_count(bk);
    let shared = cards::slot_share_count(bk);
    match kind {
        hold_kind::SLOT_HOLD if exclusive > 0 || shared > 0 => {
            return Err(format!("acquire_lease: card {card_id} unavailable (exclusive={exclusive}, shared={shared})"));
        }
        hold_kind::SLOT_SHARE if exclusive > 0 => {
            return Err(format!("acquire_lease: card {card_id} exclusively held"));
        }
        _ => {}
    }
    dispatch_hold(ctx, card_id, acquire_ms, kind, true)?;
    dispatch_hold(ctx, card_id, release_ms, kind, false)?;
    Ok(())
}

/// Apply a `±delta` to one of a soul's stat counters — the gate-owned
/// soul-stats path. The gate maps a created/destroyed/moved stat card to its
/// `(field, byte_index)` (content lives gate-side now) and pushes the delta
/// here; the module just mutates the bytes. `field`: 0=stats, 1=fatigued,
/// 2=injured. Replaces the old per-card-write `on_card_write` stat diff.
#[reducer]
pub fn set_soul_stat(
    ctx: &ReducerContext,
    soul_card_id: u32,
    field: u8,
    byte_index: u8,
    delta: i8,
    time_ms: u64,
) -> Result<(), String> {
    crate::souls::apply_stat(ctx, soul_card_id, field, byte_index, delta, time_ms);
    Ok(())
}

/// Create a card with the gate-computed `packed_def` (`[type:u4 | def_id:u12]`),
/// loose at cell (0,0) on `surface` within `macro_zone`, owned by `owner_id`,
/// stamped at `time_ms`. The id is allocated here (`next_card_id`, embedding this
/// shard). Content-agnostic — the gate resolves the def name to `packed_def` from
/// its Bundle (plan `01_gate_authority_pivot`); the module just stores it.
#[reducer]
pub fn create_card(
    ctx: &ReducerContext,
    time_ms: u64,
    packed_def: u16,
    surface: u8,
    macro_zone: u64,
    owner_id: u32,
) -> Result<(), String> {
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
    blueprint_id: u16,
) -> Result<(), String> {
    use crate::souls::soul_privates as _soul_privates_table;
    // The gate resolves the recipe's `$blueprint::<key>` ref to a Bundle id;
    // the module just sets the bit. `blueprints_0` covers ids 1..=64.
    if blueprint_id == 0 || blueprint_id > 64 {
        return Err(format!(
            "unlock_blueprint: blueprint id {blueprint_id} outside the blueprints_0 bucket (1..=64)"
        ));
    }
    let bit = 1u64 << (blueprint_id - 1);
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
