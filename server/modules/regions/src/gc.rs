//! Garbage-collection sweep for the history-style `regions` and
//! `zones` tables, plus first-publish world seeding.
//!
//! Both tables are versioned (multiple rows per identity — `macro_region`
//! for regions, `zone_id` for zones — latest wins). Neither has a
//! dead-row concept: regions/zones are re-versioned in place, never
//! tombstoned, so the rule is simply "keep the latest row per identity,
//! reap every prior version."
//!
//! Runs every `GC_INTERVAL_MS` on a recurring schedule seeded by `init`.

use std::collections::HashMap;

use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, TimeDuration};

use crate::card_shards::card_shards;
use crate::regions::regions;
use crate::zones::zones;

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
/// schedule AND the world's origin region so zones can be requested
/// against it immediately. Idempotent on both.
#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    if ctx.db.gc_schedule().iter().next().is_none() {
        ctx.db.gc_schedule().insert(GcSchedule {
            id: 0,
            scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(
                GC_INTERVAL_MS.saturating_mul(1_000),
            )),
        });
    }
    crate::regions::seed_world_region(ctx);
}

/// Periodic sweep. Reaps every non-latest `region` / `zone` version row.
///
/// Errors are not propagated — a sweep that hits unexpected state logs
/// and continues rather than getting stuck in a retry loop.
#[reducer]
pub fn gc_sweep(ctx: &ReducerContext, _row: GcSchedule) -> Result<(), String> {
    sweep_regions(ctx);
    sweep_zones(ctx);
    sweep_card_shards(ctx);
    Ok(())
}

/// `regions` sweep — prior-version reap, keyed on `macro_region`.
fn sweep_regions(ctx: &ReducerContext) {
    let mut latest_by_id: HashMap<u64, u64> = HashMap::new();
    for r in ctx.db.regions().iter() {
        latest_by_id
            .entry(r.macro_region)
            .and_modify(|m| {
                if r.valid_at > *m {
                    *m = r.valid_at;
                }
            })
            .or_insert(r.valid_at);
    }

    let mut to_delete: Vec<u64> = Vec::new();
    for r in ctx.db.regions().iter() {
        if latest_by_id.get(&r.macro_region) != Some(&r.valid_at) {
            to_delete.push(r.valid_at);
        }
    }
    for v in to_delete {
        ctx.db.regions().valid_at().delete(v);
    }
}

/// `zones` sweep — prior-version reap, keyed on `zone_id`.
fn sweep_zones(ctx: &ReducerContext) {
    let mut latest_by_id: HashMap<u32, u64> = HashMap::new();
    for z in ctx.db.zones().iter() {
        latest_by_id
            .entry(z.zone_id)
            .and_modify(|m| {
                if z.valid_at > *m {
                    *m = z.valid_at;
                }
            })
            .or_insert(z.valid_at);
    }

    let mut to_delete: Vec<u64> = Vec::new();
    for z in ctx.db.zones().iter() {
        if latest_by_id.get(&z.zone_id) != Some(&z.valid_at) {
            to_delete.push(z.valid_at);
        }
    }
    for v in to_delete {
        ctx.db.zones().valid_at().delete(v);
    }
}

/// `card_shards` sweep — prior-version reap, keyed on `data_shard`. The
/// latest version per shard is kept (even at `ref_count == 0`, a small
/// tombstone the client reads as "nothing here now").
fn sweep_card_shards(ctx: &ReducerContext) {
    let mut latest_by_id: HashMap<u16, u64> = HashMap::new();
    for r in ctx.db.card_shards().iter() {
        latest_by_id
            .entry(r.data_shard)
            .and_modify(|m| {
                if r.valid_at > *m {
                    *m = r.valid_at;
                }
            })
            .or_insert(r.valid_at);
    }

    let mut to_delete: Vec<u64> = Vec::new();
    for r in ctx.db.card_shards().iter() {
        if latest_by_id.get(&r.data_shard) != Some(&r.valid_at) {
            to_delete.push(r.valid_at);
        }
    }
    for v in to_delete {
        ctx.db.card_shards().valid_at().delete(v);
    }
}
