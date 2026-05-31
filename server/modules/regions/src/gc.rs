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

use resonantdust_content::card_model::{has_active_holds, micro_is_card, state_blocks_demotion, tile_stock};
use resonantdust_content::packed::{micro_loose_cell, unpack_definition};
use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, TimeDuration};

use crate::card_shards::card_shards;
use crate::cards::{cards, TILE_CARD_TYPE};
use crate::packed::valid_at_time;
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
    let now_ms = (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64;
    // Demote at-rest tile-cards back into their zones BEFORE the prior-version
    // reap so the freshly-written zone rows survive this sweep, and the retired
    // tile-card versions are swept next time.
    sweep_tile_card_demotions(ctx, now_ms);
    sweep_tile_cards(ctx);
    sweep_regions(ctx);
    sweep_zones(ctx);
    sweep_card_shards(ctx);
    Ok(())
}

/// Tile-card demotion sweep. Folds **at-rest** promoted tile-cards back into the
/// Zone slot they shadow, then deletes the card rows — the cross-DB analogue of
/// the monolith's `sweep_tile_card_demotions`, kept intra-`regions` because
/// tile-cards live with their zones.
///
/// A tile-card demotes only when it carries nothing the bare zone slot can't
/// express. Preconditions (all must hold):
/// - it is its `card_id`'s **latest** row, and that row is **settled**
///   (`valid_at_time ≤ now_ms`) — never fold a future-stamped, still-in-flight
///   completion into the zone early;
/// - `card_type == TILE_CARD_TYPE`;
/// - placed **loose** (not a stack member — a tile pulled under a real card is
///   in use, and its root lives in the owner-sharded `cards` DB we can't resolve);
/// - `flags_state` clean ([`state_blocks_demotion`]) and no active holds
///   ([`has_active_holds`]);
/// - a Zone exists at `card.macro_zone` whose `owner_id` matches the card's
///   (divergence implies an ownership change — preserve the card).
///
/// Demotions are **batched per zone**: all of a zone's demotable tiles fold into
/// one new Zone version via [`crate::zones::fold_tiles_at`], so the 152-byte zone
/// cost is paid once, not once per 40-byte tile.
fn sweep_tile_card_demotions(ctx: &ReducerContext, now_ms: u64) {
    // Latest row per card_id.
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

    // Per-zone fold batch: cells to write + the card_ids to fully reap on success.
    struct Batch {
        cells: Vec<(u8, u8, u16, u8, u8)>,
        retire: Vec<u32>,
    }
    let mut by_zone: HashMap<u32, Batch> = HashMap::new();

    for c in ctx.db.cards().iter() {
        if latest_by_id.get(&c.card_id) != Some(&c.valid_at) {
            continue;
        }
        if valid_at_time(c.valid_at) > now_ms {
            continue; // future-stamped completion still pending — not at rest.
        }
        let (card_type, def_id) = unpack_definition(c.packed_definition);
        if card_type != TILE_CARD_TYPE {
            continue;
        }
        if micro_is_card(c.flags_bk) {
            continue; // stacked under a card — in use, root not local.
        }
        if state_blocks_demotion(c.flags_state) || has_active_holds(c.flags_bk) {
            continue;
        }
        let Some(zone) = crate::zones::latest_for(ctx, c.macro_zone) else {
            continue;
        };
        if zone.owner_id != c.owner_id {
            continue;
        }
        let (q, r) = micro_loose_cell(c.micro_location);
        let stock0 = tile_stock(c.flags_bk, 0);
        let stock1 = tile_stock(c.flags_bk, 1);
        let batch = by_zone.entry(zone.zone_id).or_insert_with(|| Batch {
            cells: Vec::new(),
            retire: Vec::new(),
        });
        // Zone tiles index by (row=r, col=q).
        batch.cells.push((r, q, def_id, stock0, stock1));
        batch.retire.push(c.card_id);
    }

    for (zone_id, batch) in by_zone {
        // One zone version reconciles every demotable tile in it. Only retire the
        // tile-cards once the fold confirms a settled zone baseline took the data.
        if crate::zones::fold_tiles_at(ctx, zone_id, now_ms, &batch.cells).is_some() {
            for card_id in batch.retire {
                // Reap EVERY version of the demoted tile-card, not just the latest.
                // Promote-up-front leaves an earlier held now-row behind the
                // released completion row; deleting only the latest would let that
                // stale row resurface (with the pre-action stock) and mask the
                // freshly-folded zone via the client's card-priority read — the
                // "tile resets on GC" bug.
                let versions: Vec<u64> = ctx
                    .db
                    .cards()
                    .card_id()
                    .filter(card_id)
                    .map(|c| c.valid_at)
                    .collect();
                for v in versions {
                    ctx.db.cards().valid_at().delete(v);
                }
            }
        }
    }
}

/// Tile-card prior-version reap, keyed on `card_id` — same rule as the other
/// history tables (keep the latest version per id, drop the rest). Runs after
/// demotion so a just-demoted card's sole remaining row (if any) is reaped here.
fn sweep_tile_cards(ctx: &ReducerContext) {
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
        if latest_by_id.get(&c.card_id) != Some(&c.valid_at) {
            to_delete.push(c.valid_at);
        }
    }
    for v in to_delete {
        ctx.db.cards().valid_at().delete(v);
    }
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
