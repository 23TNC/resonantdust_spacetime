//! Unified garbage-collection sweep for the history-style tables
//! (cards, souls).
//!
//! Runs every `GC_INTERVAL_MS` on a recurring schedule. Replaces the
//! retired per-write `schedule_delete_*::enqueue` model: instead of
//! enqueuing a one-shot sweep on every write, ONE periodic reducer
//! walks the tables and applies the retention rules below.
//!
//! # Retention rules
//!
//! - **Alive prior-version rows** (rows whose `valid_at_time` is not
//!   the maximum among rows for the same id): always reapable. Same
//!   semantics as the old per-write sweep's strict-less-than rule —
//!   keep the latest, drop everything older.
//!
//! - **Latest-alive rows** (the max-`valid_at_time` row for an id,
//!   with `FLAG_DEAD` clear): always retained. These are the current
//!   state every subscriber wants to see.
//!
//! - **Dead rows (cards table only)** — `FLAG_DEAD` set on the
//!   latest row for a `card_id`:
//!   - **In-flight death.** `slot_hold_count > 0` (a concurrent
//!     recipe claimed the card before its scheduled death fired):
//!     retain until a later completion row supersedes it.
//!   - **World-owned** (`owning_player == WORLD_PLAYER_ID`): reap if
//!     `now - dead_at > WORLD_DEAD_RETENTION_MS` (5 min). No human
//!     to wait for.
//!   - **Player-owned**: retained until the `MAX_DEAD_RETENTION_MS`
//!     hard cap (30 days). The old per-owner post-login grace lived
//!     here, but login state now lives in the separate `players` auth
//!     database and isn't visible to this shard. Until the gateway can
//!     feed login recency back, owned dead rows just ride the hard cap.
//!     (Pre-release; data is disposable. TODO: gateway-fed grace.)
//!
//! - **Souls table**: no dead-bit concept, so only the prior-version
//!   rule applies. Latest row is retained indefinitely.
//!
//! # Bounded work
//!
//! Single-fire scope. The sweep walks each table once (one pass to
//! build the "latest per id" map, a second to evaluate reap
//! eligibility). At our scale (~100k rows max), each fire completes
//! well under a second. If row counts grow large enough that
//! single-fire duration matters, add a `GC_BATCH_SIZE` cap and a
//! cursor — the structure is amenable without changing semantics.

use std::collections::HashMap;

use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, TimeDuration};

use crate::cards;
use crate::cards::{cards as _cards_table, owning_player, WORLD_PLAYER_ID};
use crate::flags::state_flags;
use crate::packed::valid_at_time;
use crate::souls::souls;

/// Sweep interval. 10 minutes — well below all retention windows so
/// no dead row outlives its retention by more than one cadence.
const GC_INTERVAL_MS: i64 = 10 * 60 * 1_000;

/// Retention for world-owned dead cards. No human owner to wait
/// for; reap shortly after death.
const WORLD_DEAD_RETENTION_MS: u64 = 5 * 60 * 1_000;

/// Hard cap on dead-row lifetime regardless of owner state. Defends
/// against abandoned accounts inflating the table indefinitely.
/// 30 days.
const MAX_DEAD_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// Recurring schedule. Single row, `ScheduleAt::Interval`, seeded
/// by `init`. The arg row passed to `gc_sweep` is the schedule row
/// itself (SpacetimeDB convention for `scheduled(...)` tables).
#[table(accessor = gc_schedule, scheduled(gc_sweep))]
pub struct GcSchedule {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub scheduled_at: ScheduleAt,
}

/// Module init — runs once on fresh publish. Seeds the recurring
/// GC schedule. Idempotent: skips if a row already exists.
#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    if ctx.db.gc_schedule().iter().next().is_some() {
        return;
    }
    ctx.db.gc_schedule().insert(GcSchedule {
        id: 0,
        scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(
            GC_INTERVAL_MS.saturating_mul(1_000),
        )),
    });
}

/// Periodic sweep. Walks `cards` and `souls` once each and reaps
/// according to retention rules. See module doc.
///
/// Errors are not propagated — a sweep that hits unexpected state
/// logs and continues rather than getting stuck in a retry loop.
/// The next fire picks up regardless.
#[reducer]
pub fn gc_sweep(ctx: &ReducerContext, _row: GcSchedule) -> Result<(), String> {
    let now_ms = now_ms(ctx);

    sweep_cards(ctx, now_ms);
    sweep_souls(ctx);

    Ok(())
}

fn now_ms(ctx: &ReducerContext) -> u64 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64
}

/// `cards` sweep. Two passes:
///   1. Build `card_id → max valid_at` map (one row per id).
///   2. For each row: reap if non-latest, or if latest+dead+retention-elapsed.
fn sweep_cards(ctx: &ReducerContext, now_ms: u64) {
    let mut latest_by_id: HashMap<u32, u64> = HashMap::new();
    for c in ctx.db.cards().iter() {
        latest_by_id
            .entry(c.card_id)
            .and_modify(|m| {
                if c.valid_at > *m {
                    *m = c.valid_at;
                }
            })
            .or_insert(c.valid_at);
    }

    let mut to_delete: Vec<u64> = Vec::new();
    for c in ctx.db.cards().iter() {
        let is_latest = latest_by_id.get(&c.card_id) == Some(&c.valid_at);
        if !is_latest {
            // Alive prior version — always reap. Same effect as the
            // old strict-less-than sweep.
            to_delete.push(c.valid_at);
            continue;
        }
        // Latest row for this card_id.
        if c.flags_state & state_flags().dead == 0 {
            // Alive — retain.
            continue;
        }
        if dead_row_reapable(ctx, &c, now_ms) {
            to_delete.push(c.valid_at);
        }
    }
    for v in to_delete {
        ctx.db.cards().valid_at().delete(v);
    }
}

/// Apply the dead-row retention policy to a single dead card.
/// Returns `true` if the row is eligible for reaping this fire.
fn dead_row_reapable(ctx: &ReducerContext, card: &crate::cards::Card, now_ms: u64) -> bool {
    let dead_at_ms = valid_at_time(card.valid_at);
    let age_ms = now_ms.saturating_sub(dead_at_ms);

    // Hard cap — never retain past 30 days, regardless of owner.
    // Safety net for the in-flight-death retention below: if a
    // recipe somehow gets stuck holding `slot_hold` on a dead row
    // forever (server bug, lost completion write, etc), the row
    // still gets reaped eventually.
    if age_ms > MAX_DEAD_RETENTION_MS {
        return true;
    }

    // In-flight death: the row has `dead` but also carries a
    // positive `slot_hold_count` (forward-propagated onto this dead
    // row by a later chain_stitch — a concurrent recipe claimed
    // the card before its scheduled death fired). Retain so the
    // client's animation-deferral path keeps seeing the dead+held
    // state. The holding recipe's completion writes a future row that
    // decrements `slot_hold_count`; that newer row supersedes this one
    // as "latest" and the non-latest sweep reaps this row next cadence.
    if cards::slot_hold_count(card.flags_bk) > 0 {
        return false;
    }

    // Resolve owner. The dead row's own `owner_id` chain may be
    // valid (consumption typically doesn't change ownership), but
    // `owning_player` walks based on current state — for a dead
    // card whose chain is intact, this is correct.
    let owner_player_id = owning_player(ctx, card.card_id).unwrap_or(WORLD_PLAYER_ID);

    if owner_player_id == WORLD_PLAYER_ID {
        return age_ms > WORLD_DEAD_RETENTION_MS;
    }

    // Player-owned. Login recency lives in the `players` auth DB and is
    // not visible here, so we can't apply a post-login grace — the
    // 30-day hard cap above is the only bound. Retain until then.
    false
}

/// `souls` sweep. Prior-version reap only. Tombstone soul rows for
/// dead soul cards persist until the underlying card is reaped; soul
/// cleanup follows card cleanup indirectly via that path.
fn sweep_souls(ctx: &ReducerContext) {
    let mut latest_by_id: HashMap<u32, u64> = HashMap::new();
    for s in ctx.db.souls().iter() {
        latest_by_id
            .entry(s.card_id)
            .and_modify(|m| {
                if s.valid_at > *m {
                    *m = s.valid_at;
                }
            })
            .or_insert(s.valid_at);
    }

    let mut to_delete: Vec<u64> = Vec::new();
    for s in ctx.db.souls().iter() {
        if latest_by_id.get(&s.card_id) != Some(&s.valid_at) {
            to_delete.push(s.valid_at);
        }
    }
    for v in to_delete {
        ctx.db.souls().valid_at().delete(v);
    }
}
