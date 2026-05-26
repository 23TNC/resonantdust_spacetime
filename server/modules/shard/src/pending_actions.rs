//! In-flight propose_action registry — server-side dedup gate keyed on
//! the recipe + root + bindings tuple a client submitted.
//!
//! Inserted at the end of `propose_action`'s validation phase, before
//! any chain-stitch or completion-time writes land. Deleted in
//! `action_completion::commit` after the completion-time effects have
//! been emitted. While present, any subsequent `propose_action` whose
//! tuple hashes to the same `dedup_key` is rejected — catches all
//! "same exact recipe + same exact bindings" duplicates regardless of
//! whether the recipe declares a `slot_hold` claim, a `style.set`
//! channel, or any other gate.
//!
//! Private table — clients never subscribe. The gate is server-only
//! enforcement; the client's `ActionManager` predictions and the
//! row-level `slot_hold` flags are the visible signals.
//!
//! Stale rows (where `commit` never ran because of a reducer panic /
//! abort after insert) are reaped by the periodic [`crate::gc`] sweep
//! when `completion_ms + STALE_GRACE_MS < now_ms`. The grace covers
//! the longest reasonable in-flight window; rows older than that
//! couldn't possibly still be running.

use spacetimedb::{table, ReducerContext, Table};

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// One row per in-flight propose. PK is the dedup hash; a duplicate
/// tuple submission lookup is O(1).
#[table(accessor = pending_actions)]
pub struct PendingAction {
    /// Hash of `(recipe_id, root, bindings)` — see [`dedup_key`]. PK
    /// because the gate is exactly "is this tuple already in flight."
    #[primary_key]
    pub dedup_key: u64,
    /// Wall-clock ms at which `action_completion::commit` will fire
    /// for this action. The GC sweep treats rows older than
    /// `completion_ms + STALE_GRACE_MS` as orphans (commit never ran)
    /// and reaps them.
    pub completion_ms: u64,
}

/// Compute the dedup key for a (recipe, root, bindings) tuple. Same
/// inputs → same key. Collision space is 2^64 against a working-set of
/// in-flight rows that tops out around the player count — practically
/// zero collision risk. Order of bindings is preserved (iterators are
/// ordered, swapping rows would be a different proposal anyway).
pub fn dedup_key(recipe_id: u16, root: u32, bindings: &[Vec<u32>]) -> u64 {
    let mut h = DefaultHasher::new();
    recipe_id.hash(&mut h);
    root.hash(&mut h);
    bindings.hash(&mut h);
    h.finish()
}

/// Insert a registry row. Caller must have already verified no row
/// with this `dedup_key` exists via [`is_in_flight`] — this function
/// panics on PK collision (SpacetimeDB convention).
pub fn install(ctx: &ReducerContext, dedup_key: u64, completion_ms: u64) {
    ctx.db.pending_actions().insert(PendingAction {
        dedup_key,
        completion_ms,
    });
}

/// Remove the registry row for `dedup_key`. No-op if missing —
/// `commit` runs after a successful propose so the row should be
/// present, but a defensive delete is cheap and avoids brittleness if
/// a future code path skips installation.
pub fn release(ctx: &ReducerContext, dedup_key: u64) {
    if ctx.db.pending_actions().dedup_key().find(dedup_key).is_some() {
        ctx.db.pending_actions().dedup_key().delete(dedup_key);
    }
}

/// True if a registry row exists for `dedup_key`. Used by the
/// duplicate-rejection branch in `propose_action`.
pub fn is_in_flight(ctx: &ReducerContext, dedup_key: u64) -> bool {
    ctx.db.pending_actions().dedup_key().find(dedup_key).is_some()
}

/// Grace beyond `completion_ms` after which a row is presumed orphaned
/// (commit never ran — reducer panicked or aborted post-insert). The
/// GC sweep reaps anything older. 5 minutes covers any realistic
/// recipe duration plus generous server slack.
pub const STALE_GRACE_MS: u64 = 5 * 60 * 1_000;

/// Reap registry rows whose `completion_ms + STALE_GRACE_MS < now_ms`.
/// Called from the periodic [`crate::gc`] sweep. Bounded by the
/// in-flight working set so cheap even on the worst tick.
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
