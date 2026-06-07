//! Card-shard subscription index.
//!
//! A client that subscribes to this regions database needs to know
//! *which `cards` shards* hold cards positioned in this regions shard's
//! macro_zones — otherwise it would see the terrain but none of the
//! cards on it. This table is that index: a per-`data_shard` reference
//! count of cards currently here, so the client knows which `cards`
//! shards to also subscribe to.
//!
//! Subscription flow (client side):
//!   1. Subscribe to the regions DB (terrain + this table).
//!   2. Promote rows on the buffered client clock (`valid_at`, like
//!      every other history table). For each `data_shard` whose latest
//!      promoted `ref_count > 0`, subscribe to that `cards` shard; when
//!      it falls to `0`, drop that subscription.
//!
//! # Versioned counts (`valid_at`)
//!
//! Each inc/dec writes a new version row stamped with the `valid_at`
//! time of the card change that triggered it — the same time the
//! corresponding `cards` row is stamped (a recipe product's completion
//! time, a move's move time). So the count "becomes valid" exactly when
//! the card it accounts for appears on the client's buffered timeline:
//! the client promotes the subscription hint in lockstep with the card,
//! never subscribing before there's anything to see or dropping while a
//! card is still promoting in. Latest row per `data_shard`
//! (`valid_at_time ≤ now`) is the current count; the regions GC reaps
//! prior versions.
//!
//! # Maintenance — gateway-driven, derived from the validated recipe
//!
//! SpacetimeDB modules can't write across databases, so the `cards`
//! shard can't touch this table. The gateway drives it, and crucially
//! does so **entirely from the recipe it validated** — it knows what
//! each effect creates / moves / destroys, where (→ which regions
//! shard) and which `data_shard` each card belongs to. So the
//! accounting carries no per-gateway state: any gateway applying the
//! same validated recipe issues the same stamped calls.
//!   - card created in / moved into this regions shard → [`acquire_card_shard`]
//!   - card moved away / destroyed                     → [`release_card_shard`]
//! both passing the card change's `valid_at` time and its `data_shard`.
//!
//! **Authorization is the gateway's job** — these reducers trust their
//! arguments, same posture as `cards::spawn_soul`.

use spacetimedb::{reducer, table, ReducerContext, Table};

use crate::packed::{pack_valid_at, valid_at_time};
use crate::sequence;

/// Versioned per-`data_shard` reference count of cards in this regions
/// shard's coverage. Public — clients subscribe to it to learn which
/// `cards` shards to subscribe to. History-style: multiple rows per
/// `data_shard`, latest `valid_at_time ≤ now` wins.
#[table(accessor = card_shards, public)]
pub struct CardShard {
    /// Packed primary key `[time_ms:u48 | seq:u16]` — when this count
    /// becomes valid. Matches the `valid_at` of the card change that
    /// triggered it.
    #[primary_key]
    pub valid_at: u64,
    /// The `cards` module `DATA_SHARD` id (the `data_shard` column every
    /// `Card` row carries). Multiple version rows share it.
    #[index(btree)]
    pub data_shard: u16,
    /// Cards from `data_shard` referencing this regions shard as of
    /// `valid_at`. Client subscribes to `data_shard` while its latest
    /// promoted value is `> 0`.
    pub ref_count: u32,
}

/// Latest count row for `data_shard` current at `time_ms` (max
/// `valid_at_time ≤ time_ms`), or `None` if the shard has no row yet.
fn latest_at(ctx: &ReducerContext, data_shard: u16, time_ms: u64) -> Option<CardShard> {
    ctx.db
        .card_shards()
        .data_shard()
        .filter(data_shard)
        .filter(|r| valid_at_time(r.valid_at) <= time_ms)
        .max_by_key(|r| valid_at_time(r.valid_at))
}

/// Write a `(data_shard, ref_count)` version stamped at `time_ms`.
/// Mirrors `regions::write_at`: "last write at this (data_shard,
/// time_ms) wins" — same-ms rows for this shard are purged first so
/// they don't accumulate under the sequence-bearing PK.
fn write_at(ctx: &ReducerContext, data_shard: u16, ref_count: u32, time_ms: u64) {
    let stale: Vec<u64> = ctx
        .db
        .card_shards()
        .data_shard()
        .filter(data_shard)
        .filter(|r| valid_at_time(r.valid_at) == time_ms)
        .map(|r| r.valid_at)
        .collect();
    for v in stale {
        ctx.db.card_shards().valid_at().delete(v);
    }
    ctx.db.card_shards().insert(CardShard {
        valid_at: pack_valid_at(time_ms, sequence::next_sequence(ctx)),
        data_shard,
        ref_count,
    });
}

/// A card from `data_shard` was created in / moved into this regions
/// shard at `time_ms`: write a new count version = prior + 1, valid at
/// `time_ms`. `time_ms` is the card change's `valid_at` time (may be
/// future for a recipe product completing later), supplied by the
/// gateway from the validated recipe. Pair each with one later
/// [`release_card_shard`].
#[reducer]
pub fn acquire_card_shard(
    ctx: &ReducerContext,
    time_ms: u64,
    data_shard: u16,
) -> Result<(), String> {
    let prior = latest_at(ctx, data_shard, time_ms).map_or(0, |r| r.ref_count);
    write_at(ctx, data_shard, prior.saturating_add(1), time_ms);
    Ok(())
}

/// A card from `data_shard` moved away from / was destroyed in this
/// regions shard at `time_ms`: write a new count version = prior − 1
/// (floored at 0), valid at `time_ms`. The row is kept at `0` rather
/// than deleted so the timeline records when the shard went empty here
/// and the client promotes the unsubscribe at the right moment; the
/// regions GC reaps superseded versions.
#[reducer]
pub fn release_card_shard(
    ctx: &ReducerContext,
    time_ms: u64,
    data_shard: u16,
) -> Result<(), String> {
    let prior = latest_at(ctx, data_shard, time_ms).map_or(0, |r| r.ref_count);
    write_at(ctx, data_shard, prior.saturating_sub(1), time_ms);
    Ok(())
}
