//! Garbage-collection sweep for the history-style `players` table.
//!
//! Runs every `GC_INTERVAL_MS` on a recurring schedule. The `Player`
//! table is versioned (multiple rows per `player_id`, latest wins); this
//! sweep keeps the latest row per player and reaps every prior version.
//! There is no dead-row concept here — accounts aren't tombstoned, just
//! re-versioned on login / faction change — so the rule is simply
//! "drop all but the latest."
//!
//! Cards and souls are GC'd by their own `cards`-shard databases; this
//! auth DB only owns player/session/profile state.

use std::collections::HashMap;

use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, TimeDuration};

use crate::players::players;

/// Sweep interval. 10 minutes.
const GC_INTERVAL_MS: i64 = 10 * 60 * 1_000;

/// Recurring schedule. Single row, `ScheduleAt::Interval`, seeded by
/// `init`. The arg row passed to `gc_sweep` is the schedule row itself
/// (SpacetimeDB convention for `scheduled(...)` tables).
#[table(accessor = gc_schedule, scheduled(gc_sweep))]
pub struct GcSchedule {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub scheduled_at: ScheduleAt,
}

/// Module init — runs once on fresh publish. Seeds the recurring GC
/// schedule. Idempotent: skips if a row already exists.
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

/// Periodic sweep. Reaps every non-latest `Player` version row.
///
/// Errors are not propagated — a sweep that hits unexpected state logs
/// and continues rather than getting stuck in a retry loop.
#[reducer]
pub fn gc_sweep(ctx: &ReducerContext, _row: GcSchedule) -> Result<(), String> {
    sweep_players(ctx);
    Ok(())
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
