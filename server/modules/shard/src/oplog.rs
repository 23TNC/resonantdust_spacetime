//! Temporal aspect op-log — the shard table + fold readers.
//!
//! The authoritative record of future-stamped aspect operations (the holds +
//! `dead`/`reap` arcs). **Private** (not `public`): the client never subscribes —
//! it only ever sees the materialized `stock` field on `Card` rows (the fold
//! result), per the ST-only design. The server folds this log to materialize
//! those rows.
//!
//! The pure replay arithmetic lives in `resonantdust_codec::oplog`; this module
//! is the table plus the `(card, aspect)` queries that feed it. Aspect identity
//! is the global [`StockAspect`] registry (`aspect_id:u4` → stock-prefix bits),
//! so a folded value writes straight into the `stock` u64 with no content.
//!
//! See `docs/temporal_aspect_log.md`. Materialization into `Card` rows and GC
//! checkpointing are later phases; this phase is the table + append + fold.

use spacetimedb::{table, ReducerContext, Table};

use resonantdust_codec::aspects::StockAspect;
use resonantdust_codec::oplog::{fold, fold_clamped, AspectOp, Op};

/// One recorded operation against a single `(card_id, aspect_id)`. Append-only;
/// GC collapses settled rows into a single `Set` checkpoint (later phase).
///
/// `id` is a global auto-inc — it is also the **stable tiebreak** for ops sharing
/// a `time_ms` (they replay in append order, matching the within-reducer write
/// order the fold assumes).
#[table(accessor = op_log)]
pub struct OpLog {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    /// The card this op mutates.
    #[index(btree)]
    pub card_id: u32,
    /// Global aspect id — [`StockAspect::id`].
    pub aspect_id: u8,
    /// Effect time (ms). Future-stampable; the fold includes ops with
    /// `time_ms ≤ at`.
    pub time_ms: u64,
    /// Operation code — [`AspectOp::code`].
    pub op: u8,
    /// Operand: the reset value for `Set`, the magnitude for `Inc`/`Dec`.
    pub modifier: i64,
}

/// Append an op for `(card_id, aspect)` taking effect at `time_ms`. The fold
/// (and any materialization that follows) is the caller's next step — this is the
/// raw record.
pub fn append(
    ctx: &ReducerContext,
    card_id: u32,
    aspect: StockAspect,
    time_ms: u64,
    op: AspectOp,
    modifier: i64,
) {
    ctx.db.op_log().insert(OpLog {
        id: 0, // auto-inc assigns
        card_id,
        aspect_id: aspect.id(),
        time_ms,
        op: op.code(),
        modifier,
    });
}

/// Convenience: append a commutative `±1` refcount op (the holds / `dead` / `reap`
/// path). `increment` selects `Inc` vs `Dec`.
pub fn append_delta(
    ctx: &ReducerContext,
    card_id: u32,
    aspect: StockAspect,
    time_ms: u64,
    increment: bool,
) {
    let op = if increment { AspectOp::Inc } else { AspectOp::Dec };
    append(ctx, card_id, aspect, time_ms, op, 1);
}

/// Collect this `(card_id, aspect)`'s ops into the codec replay form, ordered by
/// `id` so equal-`time_ms` ops keep their append order under the fold's stable
/// sort. Bounded by the live-tail size for the card/aspect (the GC horizon).
fn ops_for(ctx: &ReducerContext, card_id: u32, aspect: StockAspect) -> Vec<Op> {
    let aid = aspect.id();
    let mut rows: Vec<OpLog> = ctx
        .db
        .op_log()
        .card_id()
        .filter(card_id)
        .filter(|r| r.aspect_id == aid)
        .collect();
    rows.sort_by_key(|r| r.id);
    rows.into_iter()
        .filter_map(|r| {
            AspectOp::from_code(r.op).map(|op| Op {
                time: r.time_ms,
                op,
                modifier: r.modifier,
            })
        })
        .collect()
}

/// The aspect's folded value as of `at` (raw, unclamped). Replays the
/// `(card, aspect)` log.
pub fn value_as_of(ctx: &ReducerContext, card_id: u32, aspect: StockAspect, at: u64) -> i64 {
    fold(&ops_for(ctx, card_id, aspect), at)
}

/// The aspect's folded value as of `at`, clamped into its stock-field width — the
/// form ready to write into a `Card`'s `stock` word via
/// [`StockAspect::field`]'s `set`.
pub fn field_value_as_of(
    ctx: &ReducerContext,
    card_id: u32,
    aspect: StockAspect,
    at: u64,
) -> u8 {
    let max = aspect.field().max() as u8;
    fold_clamped(&ops_for(ctx, card_id, aspect), at, max)
}
