//! Gate-facing write reducers — the surface the gateway calls to apply a
//! validated recipe's effects to this `cards` shard. Authorization is the
//! gateway's job: these trust their arguments (same posture as `spawn_soul`).
//!
//! Covers the dedup gate, hold acquire/release, create/destroy, and the
//! chain-stitch reposition primitives (`move_card` loose / `stack_card`
//! member) the gateway's apply step calls to reproduce `propose_action`.

use spacetimedb::{reducer, ReducerContext, Table};

use crate::cards;
use crate::flags::state_flags;
use crate::packed::with_surface;
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
    if pending_actions::is_in_flight(ctx, key, cards::now_ms(ctx)) {
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
    let field = match kind {
        hold_kind::TOUCH => HoldField::Touch,
        hold_kind::SLOT_HOLD => HoldField::SlotClaim,
        hold_kind::SLOT_SHARE => HoldField::SlotBorrow,
        hold_kind::POSITION_HOLD => HoldField::PositionHold,
        hold_kind::DROP_HOLD => HoldField::DropHold,
        hold_kind::SERVER => HoldField::Server,
        other => return Err(format!("hold: unknown kind {other}")),
    };
    if acquire {
        cards::acquire_hold(ctx, card_id, time_ms, field);
    } else {
        cards::release_hold(ctx, card_id, time_ms, field);
    }
    Ok(())
}

// ── coarse whole-action apply (one commit per shard) ─────────────────────────
//
// `apply_action` (cards data) and `apply_action_tile` (region data) each
// materialize a validated action's writes for ONE database in ONE transaction,
// so the client receives a SINGLE commit — one `now` row + one fully-formed
// `completion` row per card — instead of one commit per hold-kind / effect (the
// old `apply.rs` decomposition, which let the client promote a half-written
// completion row → the cut-tree flicker). Counts stay refcounts: these call the
// same `acquire_*`/`release_*`/`set_tile_stock` helpers, just inside one tx, so
// forward-propagation and overlap handling are unchanged. Any conflict returns
// `Err`, which rolls the whole transaction back (all-or-nothing within a shard).

/// Hold-field kinds addressable by the per-card bitmask (bit `i` = `hold_kind`
/// `i`). The gateway derives the mask from the recipe verb (use/claim/share/
/// borrow → these fields); `touch` rides along for any held card.
const HOLD_MASK_KINDS: [u8; 4] = [
    hold_kind::TOUCH,
    hold_kind::SLOT_HOLD,
    hold_kind::SLOT_SHARE,
    hold_kind::POSITION_HOLD,
];

/// CAS guard: reject if an exclusive (`slot_hold`) acquire in `mask` collides
/// with any live hold, or a shared (`slot_share`) acquire with a live exclusive.
/// Mirrors the old `acquire_lease` check, evaluated against the row current at
/// `now_ms` before any write.
fn check_hold_available(ctx: &ReducerContext, card_id: u32, mask: u8, now_ms: u64) -> Result<(), String> {
    let flags = cards::prior_at(ctx, card_id, now_ms).map(|c| c.flags).unwrap_or(0);
    let exclusive = cards::slot_claim_count(flags);
    let shared = cards::slot_borrow_count(flags);
    if mask & (1 << hold_kind::SLOT_HOLD) != 0 && (exclusive > 0 || shared > 0) {
        return Err(format!(
            "apply_action: card {card_id} unavailable (exclusive={exclusive}, shared={shared})"
        ));
    }
    if mask & (1 << hold_kind::SLOT_SHARE) != 0 && exclusive > 0 {
        return Err(format!("apply_action: card {card_id} exclusively held"));
    }
    Ok(())
}

/// Acquire (`acquire=true`) or release every hold field set in `mask` at
/// `time_ms`, via the same refcount helpers the per-kind reducers used.
fn apply_hold_mask(
    ctx: &ReducerContext,
    card_id: u32,
    mask: u8,
    time_ms: u64,
    acquire: bool,
) -> Result<(), String> {
    for kind in HOLD_MASK_KINDS {
        if mask & (1 << kind) != 0 {
            dispatch_hold(ctx, card_id, time_ms, kind, acquire)?;
        }
    }
    Ok(())
}

/// Finalize a bound card at completion: clear `pos_need`/`pos_want`. Composes
/// onto the same `completion_ms` row the releases just wrote (last-write-at-this-
/// ms wins). (Progress-bar style is no longer a per-card field — the client
/// derives it from the driving recipe.)
fn finalize_at(ctx: &ReducerContext, card_id: u32, time_ms: u64) -> Result<(), String> {
    let s = state_flags();
    cards::update_with_at(ctx, card_id, time_ms, |c| {
        c.flags &= !(s.pos_need | s.pos_want);
    })
    .ok_or_else(|| format!("apply_action: finalize card {card_id} not found"))?;
    Ok(())
}

/// Set a blueprint discovery bit on `target_card_id`'s SoulPrivate (idempotent).
fn unlock_blueprint_at(ctx: &ReducerContext, target_card_id: u32, blueprint_id: u16) -> Result<(), String> {
    use crate::souls::soul_privates as _soul_privates_table;
    if blueprint_id == 0 || blueprint_id > 64 {
        return Err(format!(
            "apply_action: blueprint id {blueprint_id} outside blueprints_0 (1..=64)"
        ));
    }
    let bit = 1u64 << (blueprint_id - 1);
    let Some(mut row) = ctx.db.soul_privates().card_id().find(target_card_id) else {
        return Err(format!("apply_action: no SoulPrivate row for target {target_card_id}"));
    };
    if row.blueprints_0 & bit != 0 {
        return Ok(());
    }
    row.blueprints_0 |= bit;
    ctx.db.soul_privates().card_id().delete(target_card_id);
    ctx.db.soul_privates().insert(row);
    Ok(())
}

/// Materialize a validated action's **cards-database** writes in one transaction:
/// every bound card's `now` row (holds acquired) and a single fully-formed
/// `completion` row (holds released + position bits cleared + progress stamped),
/// plus completion-time effects (destroy / create / unlock / soul-stat). Holds
/// and styles are parallel per-`bound_ids` arrays (`bound_masks[i]` is card `i`'s
/// hold-field bitmask, `0` for a bound-but-unheld card that only needs
/// finalizing). Effect arrays are parallel within each effect. A hold conflict
/// returns `Err` → the whole transaction rolls back.
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn apply_action(
    ctx: &ReducerContext,
    now_ms: u64,
    completion_ms: u64,
    bound_ids: Vec<u32>,
    bound_masks: Vec<u8>,
    destroy_ids: Vec<u32>,
    create_defs: Vec<u16>,
    create_surfaces: Vec<u8>,
    create_macro_zones: Vec<u64>,
    create_owners: Vec<u32>,
    // Per-created-card initial stock u32 (gate-computed `@define` defaults).
    create_stocks: Vec<u32>,
    unlock_targets: Vec<u32>,
    unlock_blueprints: Vec<u16>,
    stat_souls: Vec<u32>,
    stat_fields: Vec<u8>,
    stat_bytes: Vec<u8>,
    stat_deltas: Vec<i8>,
    // Per-card `stock` u32 writes (gate-computed absolute values).
    stock_card_ids: Vec<u32>,
    stock_values: Vec<u32>,
    // Stack-splice repositions: members of a destroyed root, re-rooted so none is
    // left pointing at a dead root. Parallel arrays; `stack_state` is the u8
    // `[stack_id:u4|index:u4]` (0 = loose). Applied @completion (coalesces with a
    // member's own hold-release/finalize row).
    reroot_ids: Vec<u32>,
    reroot_macro_zones: Vec<u64>,
    reroot_micro_locations: Vec<u32>,
    reroot_stack_states: Vec<u8>,
) -> Result<(), String> {
    // 1. CAS pre-check across every bound card — reject before any write (the
    //    rollback-on-Err makes the ordering immaterial, but pre-checking keeps
    //    the failure clean and matches the old fail-fast semantics).
    for (i, &id) in bound_ids.iter().enumerate() {
        check_hold_available(ctx, id, bound_masks.get(i).copied().unwrap_or(0), now_ms)?;
    }
    // 2. Per bound card: acquire @now, release + finalize @completion. The two
    //    rows survive; in-transaction rewrites coalesce (no extra commits).
    for (i, &id) in bound_ids.iter().enumerate() {
        let mask = bound_masks.get(i).copied().unwrap_or(0);
        apply_hold_mask(ctx, id, mask, now_ms, true)?;
        apply_hold_mask(ctx, id, mask, completion_ms, false)?;
        finalize_at(ctx, id, completion_ms)?;
    }
    // 3. Completion-time effects.
    let dead = state_flags().dead;
    for &id in &destroy_ids {
        cards::update_with_at(ctx, id, completion_ms, |c| c.flags |= dead)
            .ok_or_else(|| format!("apply_action: destroy card {id} not found"))?;
    }
    for i in 0..create_defs.len() {
        let new_id = cards::next_card_id(ctx);
        cards::create_at(
            ctx,
            new_id,
            completion_ms,
            with_surface(create_macro_zones[i], create_surfaces[i]),
            cards::Micro::snap(0, 0),
            create_owners[i],
            create_defs[i],
            /* flags */ 0,
            create_stocks.get(i).copied().unwrap_or(0),
        );
    }
    for i in 0..unlock_targets.len() {
        unlock_blueprint_at(ctx, unlock_targets[i], unlock_blueprints[i])?;
    }
    for i in 0..stat_souls.len() {
        crate::souls::apply_stat(
            ctx,
            stat_souls[i],
            stat_fields[i],
            stat_bytes[i],
            stat_deltas[i],
            completion_ms,
        );
    }
    // Per-card stock writes (absolute, gate-computed). Future-stamped @completion
    // so the new stock lands on the client's timeline like every other effect.
    for i in 0..stock_card_ids.len() {
        let id = stock_card_ids[i];
        let value = stock_values[i];
        cards::update_with_at(ctx, id, completion_ms, |c| c.stock = value)
            .ok_or_else(|| format!("apply_action: stock card {id} not found"))?;
    }
    // Stack-splice repositions @completion (after destroys, so the orphaned
    // members re-root as the destroyed root's row goes dead in the same commit).
    // Best-effort: a member that vanished concurrently is simply skipped.
    for i in 0..reroot_ids.len() {
        use crate::cards::MicroPlace;
        let micro = cards::Micro::of(reroot_micro_locations[i], reroot_stack_states[i] as u32);
        cards::update_with_at(ctx, reroot_ids[i], completion_ms, |c| {
            c.macro_zone = reroot_macro_zones[i];
            micro.place(c);
        });
    }
    Ok(())
}

/// Materialize a validated action's **region-database** (tile) writes in one
/// transaction: promote the tile + acquire its masked holds @`now_ms`, then
/// release + apply the stock effects @`completion_ms`, so the tile-card lands as
/// one `now` row + one fully-formed `completion` row (born decremented). `slot_hold`
/// in the mask is the concurrent-cut guard — a conflict returns `Err` and rolls
/// the transaction back. Stock arrays are parallel.
#[reducer]
#[allow(clippy::too_many_arguments)]
pub fn apply_action_tile(
    ctx: &ReducerContext,
    now_ms: u64,
    completion_ms: u64,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    hold_mask: u8,
    stock_slots: Vec<u8>,
    stock_ops: Vec<u8>,
    stock_deltas: Vec<u8>,
) -> Result<(), String> {
    // Acquire each masked hold @now — promotes the tile-card and (for slot_hold)
    // runs the exclusive CAS.
    for kind in HOLD_MASK_KINDS {
        if hold_mask & (1 << kind) != 0 {
            crate::tiles::acquire_tile_hold(ctx, surface, macro_zone, q, r, hold_field(kind)?, now_ms)?;
        }
    }
    // Release @completion.
    for kind in HOLD_MASK_KINDS {
        if hold_mask & (1 << kind) != 0 {
            crate::tiles::release_tile_hold(ctx, surface, macro_zone, q, r, hold_field(kind)?, completion_ms);
        }
    }
    // Stock effects @completion — read-modify-write per slot so the completion
    // row is born with the decremented value (no stale intermediate).
    for i in 0..stock_slots.len() {
        let tile = crate::tiles::find_or_create_tile_card(ctx, surface, macro_zone, q, r, completion_ms)?;
        let slot = stock_slots[i] as usize;
        let current = stock(tile.stock, slot);
        let next = match stock_ops[i] {
            stock_op::SUB => current.saturating_sub(stock_deltas[i]),
            stock_op::ADD => current.saturating_add(stock_deltas[i]).min(0b11),
            stock_op::SET => stock_deltas[i].min(0b11),
            other => return Err(format!("apply_action_tile: unknown stock op {other}")),
        };
        crate::tiles::set_tile_stock(ctx, tile.card_id, completion_ms, slot, next);
    }
    Ok(())
}

// ── deployment identity ─────────────────────────────────────────────────────

/// Seed (or overwrite) this deployment's [`crate::cards::ShardIdentity`]. The
/// unified data module is published to both the owner-card DBs and the region
/// DBs; the gate calls this once after publishing to a **region** DB so its
/// tile-card ids carry `CARD_DB_REGIONS`. A card DB needs no call — the unseeded
/// default is `(CARD_DB_CARDS, DATA_SHARD)`.
#[reducer]
pub fn set_shard_identity(ctx: &ReducerContext, card_db: u8, shard: u16) -> Result<(), String> {
    cards::set_identity(ctx, card_db, shard);
    Ok(())
}

// ── tile (region-DB) reducers ───────────────────────────────────────────────
// Folded in from the former `regions` module. Same binary serves both DB
// families, so these ride along (unused on a card DB). They operate on the
// canonical `Card` table via `crate::tiles`, so tile-cards get the full
// bitemporal write semantics (forward-prop included) the owner-card path uses.

use resonantdust_codec::card_model::{stock, HoldField};

/// Tile-stock op codes shared with the gateway's apply step.
mod stock_op {
    pub const SUB: u8 = 0;
    pub const ADD: u8 = 1;
    pub const SET: u8 = 2;
}

/// Hold-kind selector → [`HoldField`]. Matches `hold_kind` above and the
/// gateway (`0=touch, 1=slot_hold, 2=slot_share, 3=position_hold`).
fn hold_field(kind: u8) -> Result<HoldField, String> {
    Ok(match kind {
        0 => HoldField::Touch,
        1 => HoldField::SlotClaim,
        2 => HoldField::SlotBorrow,
        3 => HoldField::PositionHold,
        other => return Err(format!("tile hold: unknown kind {other}")),
    })
}

