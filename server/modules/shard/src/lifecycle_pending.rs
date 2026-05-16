//! Server-side detail table tracking active magnetic actions per
//! player. Populated by `cards::write_at` when a magnetic card is
//! created, cleaned up when it dies. The companion summary fields
//! (`lifecycle_count`, `earliest_lifecycle_expires_ms`) live on
//! `PlayerProfile` so the hot-path block check in `propose_action`
//! reads one row, not two.
//!
//! Private table — clients never subscribe. The client learns about
//! its own magnetic state via the `FLAG_MAGNETIC_HOLD` flag on cards
//! (visible through normal card subscriptions). The server's view
//! exists purely to enforce the blocking gate on expired actions.
//!
//! See [docs/MAGNETIC_REWRITE.md](../../../../../docs/MAGNETIC_REWRITE.md)
//! for the broader design.

use spacetimedb::{table, ReducerContext, Table};

use crate::cards::cards;
use crate::players::player_profiles;

/// One row per active magnetic action. Deleted at the moment the
/// magnetic card transitions to dead=1 (recipe consumed it). Stale
/// rows pointing at non-existent / dead cards can briefly exist after
/// out-of-band card deletions (e.g. `players::delete_player` cascade);
/// the block-check path in `actions::propose_action` purges them
/// defensively when it encounters them.
#[table(accessor = lifecycle_pending)]
pub struct LifecyclePending {
    /// The magnetic card's id. PK because a card can only have one
    /// active magnetic phase at a time (def-driven; flag-inheritance
    /// is binary).
    #[primary_key]
    pub card_id: u32,
    /// Wall-clock ms at which this phase ends. Computed as
    /// `install_row.valid_at_time + def.magnetic_duration_ms`. Used by
    /// the block-check to determine if the action is overdue (and by
    /// how much).
    pub expires_at_ms: u64,
    /// The player "responsible" for resolving this magnetic action —
    /// the result of walking `owning_player` from the magnetic card
    /// at install time. `WORLD_PLAYER_ID` (= 0) for world-owned
    /// magnetics, which the block-check filters out (no one to
    /// block). Indexed so the cleanup / re-summarize path can find
    /// all of a player's rows in one btree query.
    #[index(btree)]
    pub player_id: u32,
}

/// Insert (or replace) the detail row for `card_id`. Called from
/// `cards::write_at` on the magnetic-install path. Idempotent: if a
/// row already exists for this card_id, it's overwritten — the new
/// install supersedes any prior install (which shouldn't happen for
/// a single card, but defending against re-spawn races is cheap).
pub fn install(ctx: &ReducerContext, card_id: u32, expires_at_ms: u64, player_id: u32) {
    // The table uses `card_id` as PK; `find` + `delete` clears any
    // existing row before insert. Otherwise the insert would error
    // on a duplicate PK.
    if ctx.db.lifecycle_pending().card_id().find(card_id).is_some() {
        ctx.db.lifecycle_pending().card_id().delete(card_id);
    }
    ctx.db.lifecycle_pending().insert(LifecyclePending {
        card_id,
        expires_at_ms,
        player_id,
    });
}

/// Remove the detail row for `card_id` if present. Called from
/// `cards::write_at` on the dead-transition path. No-op if no row
/// exists (the card wasn't magnetic, or its row was already removed
/// by an earlier transition).
pub fn remove(ctx: &ReducerContext, card_id: u32) {
    if ctx.db.lifecycle_pending().card_id().find(card_id).is_some() {
        ctx.db.lifecycle_pending().card_id().delete(card_id);
    }
}

/// Compute the earliest `expires_at_ms` (and count) of all detail
/// rows for `player_id`. Used to re-summarize `PlayerProfile` after
/// any change. `Ok(None)` semantics — `(0, 0)` means "no active
/// magnetic actions" (count=0, earliest=0 sentinel).
pub fn summarize_for_player(ctx: &ReducerContext, player_id: u32) -> (u32, u64) {
    let mut count: u32 = 0;
    let mut earliest: u64 = u64::MAX;
    for row in ctx.db.lifecycle_pending().player_id().filter(player_id) {
        count = count.saturating_add(1);
        if row.expires_at_ms < earliest {
            earliest = row.expires_at_ms;
        }
    }
    if count == 0 {
        (0, 0)
    } else {
        (count, earliest)
    }
}

// ---------- Blocking gate -------------------------------------------------

/// Grace period (ms) after a magnetic action's `expires_at_ms` before
/// the blocking gate engages. Within this window the caller can still
/// do other things; past it, only calls that resolve one of their
/// blocked cards are accepted.
///
/// 60 seconds. The window absorbs honest client lag (network latency,
/// promote-buffer delay, on-login enumeration) while still bounding
/// how long a stalling client can drag out unresolved magnetic
/// actions.
const BLOCK_GRACE_MS: u64 = 60_000;

/// Error string prefix the client matches on to detect block-error
/// responses. Format:
///
/// ```text
/// magnetic_blocked: card_id=<id>; expires_at_ms=<ms>; overdue_ms=<ms>
/// ```
///
/// One offending card per error — the earliest-expiring one. Once
/// resolved, the next call surfaces the next-earliest (or succeeds
/// if there are no more expired actions).
const BLOCK_ERROR_PREFIX: &str = "magnetic_blocked:";

/// Check whether the caller is currently blocked by an expired
/// magnetic action.
///
/// `involved_card_ids` is the set of card_ids the calling reducer is
/// touching as part of its action — typically `[hex, root, ...slots]`
/// for `propose_action`, or empty for reducers that don't reference
/// existing cards (`character_creation`, `deploy_mini_zone`).
///
/// Returns:
///
/// - `Ok(())` — caller has no expired magnetic actions, OR all expired
///   actions are still within `BLOCK_GRACE_MS`, OR the call references
///   one of the caller's blocked cards (resolution attempt).
/// - `Err(String)` prefixed with `BLOCK_ERROR_PREFIX` — caller is
///   blocked. The error names the earliest-expiring blocked card so
///   the client knows what to resolve.
///
/// Defensive: if the `lifecycle_pending` row references a card that no
/// longer exists (e.g. swept by GC, deleted via admin path), it's
/// purged inline and the player's profile re-summarized before
/// re-evaluating the gate. Prevents phantom blocks from stuck state.
pub fn block_check(
    ctx: &ReducerContext,
    caller_player_id: u32,
    now_ms: u64,
    involved_card_ids: &[u32],
) -> Result<(), String> {
    // Hot path: read the summary off PlayerProfile. The vast majority
    // of callers have no active magnetic actions and short-circuit
    // here without touching the detail table.
    let Some(profile) = ctx
        .db
        .player_profiles()
        .player_id()
        .find(caller_player_id)
    else {
        return Ok(());
    };
    if profile.lifecycle_count == 0 {
        return Ok(());
    }
    if now_ms < profile.earliest_lifecycle_expires_ms.saturating_add(BLOCK_GRACE_MS) {
        // Even the earliest pending action isn't past its grace yet.
        return Ok(());
    }

    // Past the grace — engage the gate. Walk this caller's detail
    // rows to find an offending (expired) entry, AND check the
    // carve-out (does the caller's involved set reference any
    // blocked card?).
    let mut offending: Option<LifecyclePending> = None;
    let mut needs_resummarize = false;
    for row in ctx
        .db
        .lifecycle_pending()
        .player_id()
        .filter(caller_player_id)
    {
        // Stale-row purge: if the referenced card is gone, the
        // pending row is phantom state. Drop it and continue.
        if ctx.db.cards().card_id().filter(row.card_id).next().is_none() {
            ctx.db.lifecycle_pending().card_id().delete(row.card_id);
            needs_resummarize = true;
            continue;
        }
        // Carve-out: caller's call references this blocked card.
        // Treat as a resolution attempt and let the call proceed.
        if involved_card_ids.contains(&row.card_id) {
            if needs_resummarize {
                crate::players::resync_lifecycle_summary(ctx, caller_player_id);
            }
            return Ok(());
        }
        // Track the earliest-expired row for the error message.
        if row.expires_at_ms.saturating_add(BLOCK_GRACE_MS) <= now_ms {
            match &offending {
                None => offending = Some(row),
                Some(o) if row.expires_at_ms < o.expires_at_ms => offending = Some(row),
                _ => {}
            }
        }
    }

    if needs_resummarize {
        crate::players::resync_lifecycle_summary(ctx, caller_player_id);
    }

    match offending {
        None => Ok(()),
        Some(row) => {
            let overdue_ms = now_ms.saturating_sub(row.expires_at_ms);
            Err(format!(
                "{} card_id={}; expires_at_ms={}; overdue_ms={}",
                BLOCK_ERROR_PREFIX, row.card_id, row.expires_at_ms, overdue_ms,
            ))
        }
    }
}
