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

use resonantdust_codec::card_model::{
    has_active_holds, micro_is_card, state_blocks_demotion, stock, STOCK_ZONE_SAVE_MASK,
};

use crate::card_shards::card_shards;
use crate::cards;
use crate::cards::{cards as _cards_table, owning_player, WORLD_PLAYER_ID};
use crate::flags::state_flags;
use crate::packed::{micro_loose_cell, unpack_definition, valid_at_time};
use crate::regions::regions;
use crate::souls::souls;
use crate::tiles::TILE_CARD_TYPE;
use crate::zones::zones;

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
    if ctx.db.gc_schedule().iter().next().is_none() {
        ctx.db.gc_schedule().insert(GcSchedule {
            id: 0,
            scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(
                GC_INTERVAL_MS.saturating_mul(1_000),
            )),
        });
    }
    // Seed the world's origin region so zones can be requested immediately. On a
    // card DB this just lands one unused `regions` row (the region tables are
    // empty there) — harmless; init runs before the gate seeds `ShardIdentity`,
    // so it can't be role-gated. Idempotent.
    crate::regions::seed_world_region(ctx);
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

    // Demote at-rest tile-cards back into their zones BEFORE the card reap so the
    // freshly-written zone rows survive and the retired tile-card versions are
    // reaped by `sweep_cards`. No-op on a card DB (no tile-cards / zones).
    sweep_tile_card_demotions(ctx, now_ms);
    // `sweep_cards` covers BOTH owner cards and tile-cards (tile-cards never set
    // `dead`, so for them it's a plain prior-version reap) — the former
    // `sweep_tile_cards` is subsumed.
    sweep_cards(ctx, now_ms);
    sweep_souls(ctx);
    // Region-DB tables. Prior-version reaps; no-op where empty (a card DB).
    // `regions` is current-value (one row per macro_region) — nothing to reap.
    sweep_zones(ctx);
    sweep_card_shards(ctx);

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
        if c.flags & state_flags().dead == 0 {
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
    if cards::slot_claim_count(card.flags) > 0 {
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

// ── region-DB sweeps (folded from the former `regions` module) ───────────────
// These walk the region-only tables. On a card-DB deployment those tables are
// empty, so each is a no-op.

/// Tile-card demotion sweep. Folds **at-rest** promoted tile-cards back into the
/// Zone slot they shadow, then deletes the card rows. A tile-card demotes only
/// when it carries nothing the bare zone slot can't express: it is its
/// `card_id`'s latest, settled (`valid_at_time ≤ now`) row; `card_type ==
/// TILE_CARD_TYPE`; placed loose (not a stack member); `flags_state` clean and
/// no active holds; and a Zone exists at its `macro_zone` with matching owner.
/// Demotions batch per zone — all of a zone's demotable tiles fold into one new
/// Zone version, paying the zone cost once.
fn sweep_tile_card_demotions(ctx: &ReducerContext, now_ms: u64) {
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

    // Per-zone fold batch: cells to write + the card_ids to fully reap on
    // success + the time to stamp the folded zone row at.
    struct Batch {
        cells: Vec<(u8, u8, u16, u8, u8)>,
        retire: Vec<u32>,
        // Back-date the fold to the LATEST demoted-card `valid_at_time` in this
        // batch, NOT `now_ms`. Every demoted card is settled (`≤ now_ms`) and was
        // being shown by clients, so its `valid_at` is already in their promoted
        // past — stamping the new zone baseline there makes it promotable the
        // instant the card is reaped. Stamping at `now_ms` (a client-future row
        // they can't promote yet) is the GC fold-back flash: the tile snaps to the
        // stale pre-card zone, then to the new baseline a buffer-length later.
        fold_time: u64,
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
        if micro_is_card(c.flags) {
            continue; // stacked under a card — in use, root not local.
        }
        if state_blocks_demotion(c.flags) || has_active_holds(c.flags) {
            continue;
        }
        // Demote-guard: a zone tile only persists the bottom u4
        // (`STOCK_ZONE_SAVE_MASK`). If any of the upper 60 stock bits are set, the
        // card carries transient state (e.g. in-progress build) the zone can't
        // express — keep the card alive until it returns to 0. Content-free: the
        // shard never needs the def's defaults, just "non-savable bits are clear".
        if c.stock & !STOCK_ZONE_SAVE_MASK != 0 {
            continue;
        }
        let Some(zone) = crate::zones::latest_for(ctx, c.macro_zone) else {
            continue;
        };
        if zone.owner_id != c.owner_id {
            continue;
        }
        let (q, r) = micro_loose_cell(c.micro_location);
        let stock0 = stock(c.stock, 0);
        let stock1 = stock(c.stock, 1);
        let batch = by_zone.entry(zone.zone_id).or_insert_with(|| Batch {
            cells: Vec::new(),
            retire: Vec::new(),
            fold_time: 0,
        });
        batch.fold_time = batch.fold_time.max(valid_at_time(c.valid_at));
        // Zone tiles index by (row=r, col=q).
        batch.cells.push((r, q, def_id, stock0, stock1));
        batch.retire.push(c.card_id);
    }

    for (zone_id, batch) in by_zone {
        // One zone version reconciles every demotable tile in it. Only retire the
        // tile-cards once the fold confirms a settled zone baseline took the data.
        // Stamped at the batch's latest demoted-card time (back-dated, see Batch).
        if crate::zones::fold_tiles_at(ctx, zone_id, batch.fold_time, &batch.cells).is_some() {
            for card_id in batch.retire {
                // Reap EVERY version of the demoted tile-card, not just the latest —
                // a promote-up-front held now-row left behind the released
                // completion row would otherwise resurface with the pre-action
                // stock and mask the freshly-folded zone.
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

/// `card_shards` sweep — prior-version reap, keyed on `data_shard`. The latest
/// version per shard is kept (even at `ref_count == 0`, a tombstone the client
/// reads as "nothing here now").
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
