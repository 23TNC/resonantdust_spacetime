//! In-flight action dedup registry — the DB-side race guard the gateway
//! relies on. Keyed on the `(recipe_id, root, bindings)` tuple the gate
//! validated, installed before any write-back lands and released after the
//! completion-time effects are written. While present, a second proposal that
//! hashes to the same key is rejected (see `gate_api::claim_pending`).
//!
//! Ported from the monolithic `shard` module. Private table — clients never
//! subscribe; this is server-only enforcement. Stale rows (where release never
//! ran after an aborted apply) are reaped by the GC sweep via [`sweep_stale`].

use spacetimedb::{table, ReducerContext, Table};

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// One row per in-flight action. PK is the dedup hash, so the
/// already-in-flight lookup is O(1).
#[table(accessor = pending_actions)]
pub struct PendingAction {
    #[primary_key]
    pub dedup_key: u64,
    /// Wall-clock ms the action's completion effects are stamped at; the GC
    /// sweep reaps rows older than `completion_ms + STALE_GRACE_MS`.
    pub completion_ms: u64,
}

/// Dedup key for a `(recipe, root, bindings)` tuple — same inputs, same key.
/// Computed server-side (here) so the hash is consistent regardless of the
/// caller; the gateway passes the raw tuple, never a pre-hashed key.
pub fn dedup_key(recipe_id: u16, root: u32, bindings: &[Vec<u32>]) -> u64 {
    let mut h = DefaultHasher::new();
    recipe_id.hash(&mut h);
    root.hash(&mut h);
    bindings.hash(&mut h);
    h.finish()
}

/// Insert a registry row. Caller must have checked [`is_in_flight`] first —
/// inserting a duplicate PK panics (SpacetimeDB convention).
pub fn install(ctx: &ReducerContext, dedup_key: u64, completion_ms: u64) {
    ctx.db.pending_actions().insert(PendingAction {
        dedup_key,
        completion_ms,
    });
}

/// Remove the registry row for `dedup_key` (no-op if missing).
pub fn release(ctx: &ReducerContext, dedup_key: u64) {
    if ctx.db.pending_actions().dedup_key().find(dedup_key).is_some() {
        ctx.db.pending_actions().dedup_key().delete(dedup_key);
    }
}

/// True if a registry row exists for `dedup_key`.
pub fn is_in_flight(ctx: &ReducerContext, dedup_key: u64) -> bool {
    ctx.db.pending_actions().dedup_key().find(dedup_key).is_some()
}

/// Grace beyond `completion_ms` after which a row is presumed orphaned.
pub const STALE_GRACE_MS: u64 = 5 * 60 * 1_000;

/// Reap registry rows whose `completion_ms + STALE_GRACE_MS < now_ms`. Wire
/// into the cards GC sweep.
pub fn sweep_stale(ctx: &ReducerContext, now_ms: u64) {
    let cutoff = now_ms.saturating_sub(STALE_GRACE_MS);
    let stale: Vec<u64> = ctx
        .db
        .pending_actions()
        .iter()
        .filter(|r| r.completion_ms < cutoff)
        .map(|r| r.dedup_key)
        .collect();
    for key in stale {
        ctx.db.pending_actions().dedup_key().delete(key);
    }
}
