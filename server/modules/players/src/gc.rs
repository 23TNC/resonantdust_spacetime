//! Unified garbage-collection sweep for the history-style tables
//! (cards, players, souls).
//!
//! Runs every `GC_INTERVAL_MS` on a recurring schedule. Replaces the
//! retired per-write `schedule_delete_*::enqueue` model: instead of
//! enqueuing a one-shot sweep on every write, ONE periodic reducer
//! walks the three tables and applies the retention rules below.
//!
//! See [docs/MAGNETIC_REWRITE.md Phase 4](../../../../../docs/MAGNETIC_REWRITE.md)
//! for the design rationale.
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
//!   - **Hard cap.** If `now - dead_at > MAX_DEAD_RETENTION_MS` (30
//!     days): reap. Backstop for abandoned accounts.
//!   - **World-owned** (`owning_player == WORLD_PLAYER_ID`): reap if
//!     `now - dead_at > WORLD_DEAD_RETENTION_MS` (5 min). No human
//!     to wait for.
//!   - **Owner currently online + past post-login grace** (logged in
//!     ≥ `POST_LOGIN_GRACE_MS` ago): reap. Owner has had time to
//!     reconcile.
//!   - **Owner offline OR within post-login grace**: retain. The
//!     dead row is what the offline-reconciling client will see on
//!     login.
//!
//! - **Players / souls tables**: no dead-bit concept, so only the
//!   prior-version rule applies. Latest row is retained
//!   indefinitely (matches existing `schedule_delete_*` semantics).
//!
//! # Bounded work
//!
//! Single-fire scope. The sweep walks each table once (one pass to
//! build the "latest per id" map, a second to evaluate reap
//! eligibility). At our scale (~100k rows max), each fire completes
//! well under a second. If row counts grow large enough that
//! single-fire duration matters, add a `GC_BATCH_SIZE` cap and a
//! cursor — the structure is amenable without changing semantics.

use std::collections::{HashMap, HashSet};

use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, TimeDuration};

use crate::cards;
use crate::cards::{cards as _cards_table, owning_player, WORLD_PLAYER_ID};
use crate::flags::state_flags;
use crate::packed::valid_at_time;
use crate::players::{player_sessions, players};
use crate::souls::souls;

/// Sweep interval. 10 minutes — well below all retention windows so
/// no dead row outlives its retention by more than one cadence.
const GC_INTERVAL_MS: i64 = 10 * 60 * 1_000;

/// Post-login grace: dead rows owned by a player who logged in less
/// than this long ago are retained, giving the returning client time
/// to enumerate / reconcile before the sweep takes them away.
const POST_LOGIN_GRACE_MS: u64 = 5 * 60 * 1_000;

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

/// Periodic sweep. Walks `cards`, `players`, `souls` once each and
/// reaps according to retention rules. See module doc.
///
/// Errors are not propagated — a sweep that hits unexpected state
/// logs and continues rather than getting stuck in a retry loop.
/// The next fire picks up regardless.
#[reducer]
pub fn gc_sweep(ctx: &ReducerContext, _row: GcSchedule) -> Result<(), String> {
    let now_ms = now_ms(ctx);

    // Pre-load player state once per fire so the per-row check is
    // O(1) hash lookups instead of repeated table scans.
    let logged_in = logged_in_set(ctx);
    let last_login = last_login_map(ctx);

    sweep_cards(ctx, now_ms, &logged_in, &last_login);
    sweep_players(ctx);
    sweep_souls(ctx);

    Ok(())
}

fn now_ms(ctx: &ReducerContext) -> u64 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64
}

/// Build the set of `player_id`s currently holding a `player_sessions`
/// row. One identity per session, so the same `player_id` can appear
/// multiple times (one per active client); the `HashSet` dedups.
fn logged_in_set(ctx: &ReducerContext) -> HashSet<u32> {
    ctx.db
        .player_sessions()
        .iter()
        .map(|s| s.player_id)
        .collect()
}

/// Build `player_id → last_login_secs` from the latest row per
/// player. `last_login_secs` lives on the history-style `Player`
/// table, so we have to find the max-`valid_at_time` row per
/// `player_id`. One pass over the table.
fn last_login_map(ctx: &ReducerContext) -> HashMap<u32, u32> {
    let mut latest_by_player: HashMap<u32, (u64, u32)> = HashMap::new();
    for p in ctx.db.players().iter() {
        let t = valid_at_time(p.valid_at);
        latest_by_player
            .entry(p.player_id)
            .and_modify(|(mt, ls)| {
                if t > *mt {
                    *mt = t;
                    *ls = p.last_login_secs;
                }
            })
            .or_insert((t, p.last_login_secs));
    }
    latest_by_player
        .into_iter()
        .map(|(k, (_, ls))| (k, ls))
        .collect()
}

/// `cards` sweep. Two passes:
///   1. Build `card_id → max valid_at` map (one row per id).
///   2. For each row: reap if non-latest, or if latest+dead+retention-elapsed.
fn sweep_cards(
    ctx: &ReducerContext,
    now_ms: u64,
    logged_in: &HashSet<u32>,
    last_login: &HashMap<u32, u32>,
) {
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
        if dead_row_reapable(ctx, &c, now_ms, logged_in, last_login) {
            to_delete.push(c.valid_at);
        }
    }
    for v in to_delete {
        ctx.db.cards().valid_at().delete(v);
    }
}

/// Apply the dead-row retention policy to a single dead card.
/// Returns `true` if the row is eligible for reaping this fire.
fn dead_row_reapable(
    ctx: &ReducerContext,
    card: &crate::cards::Card,
    now_ms: u64,
    logged_in: &HashSet<u32>,
    last_login: &HashMap<u32, u32>,
) -> bool {
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
    // state. The holding recipe's `action_completion` writes a
    // future row at completion that decrements `slot_hold_count`;
    // that newer row supersedes this one as "latest" and the
    // non-latest sweep reaps this row on the next cadence.
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

    if !logged_in.contains(&owner_player_id) {
        // Offline owner: retain so they can reconcile on login.
        // The MAX_DEAD_RETENTION_MS hard cap above handles never-
        // returning players.
        return false;
    }

    // Owner is logged in. Reap iff they've been online long enough
    // that the post-login grace has elapsed. `last_login_secs` is
    // in unix seconds; convert.
    let Some(&last_login_secs) = last_login.get(&owner_player_id) else {
        // No profile / no record — treat as "just logged in,"
        // give them the grace window.
        return false;
    };
    let last_login_ms = (last_login_secs as u64).saturating_mul(1_000);
    let time_since_login_ms = now_ms.saturating_sub(last_login_ms);
    time_since_login_ms > POST_LOGIN_GRACE_MS
}

/// `players` sweep. No dead-row concept on this table — just the
/// prior-version rule.
fn sweep_players(ctx: &ReducerContext) {
    let mut latest_by_id: HashMap<u32, u64> = HashMap::new();
    for p in ctx.db.players().iter() {
        latest_by_id
            .entry(p.player_id)
            .and_modify(|m| {
                if p.valid_at > *m {
                    *m = p.valid_at;
                }
            })
            .or_insert(p.valid_at);
    }

    let mut to_delete: Vec<u64> = Vec::new();
    for p in ctx.db.players().iter() {
        if latest_by_id.get(&p.player_id) != Some(&p.valid_at) {
            to_delete.push(p.valid_at);
        }
    }
    for v in to_delete {
        ctx.db.players().valid_at().delete(v);
    }
}

/// `souls` sweep. Same shape as players — prior-version reap only.
/// Tombstone soul rows for dead soul cards persist until the
/// underlying card is reaped; soul cleanup follows card cleanup
/// indirectly via that path.
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
