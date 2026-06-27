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

use crate::cards::cards as _cards_table;
use crate::packed::valid_at_time;
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

/// Re-materialize `aspect` onto `card_id` from `from_time` forward: fold the log
/// and write the folded value into the `stock` aspect-field of the row current at
/// `from_time` (producing a new row at that stamp) and of every future-stamped
/// row. This is the op-log's replacement for `cards::propagate_hold_forward` —
/// same forward walk + bypass discipline, but the value comes from the general
/// fold (so it's correct for value-dependent ops, not just commutative ±1).
///
/// The future-row rewrites bypass `cards::write_at` (direct delete/insert,
/// preserving the `valid_at` PK) so we don't re-fire the souls / flag-diff /
/// cascade hooks while restamping rows we already wrote — exactly as
/// `propagate_hold_forward` does. The row current at `from_time` DOES go through
/// `write_at`, so its dirty markers + soul stat diff update normally.
pub fn materialize_aspect(
    ctx: &ReducerContext,
    card_id: u32,
    aspect: StockAspect,
    from_time: u64,
) {
    let field = aspect.field();

    // Row current at `from_time` → a new row carrying the folded value. No-op if
    // the card has no row at/before `from_time` yet (nothing to materialize onto).
    let v = field_value_as_of(ctx, card_id, aspect, from_time);
    crate::cards::update_with_at(ctx, card_id, from_time, |c| {
        c.stock = field.set(c.stock, v);
    });

    // Future-stamped rows: re-fold each at its own time and rewrite its field.
    let future: Vec<u64> = ctx
        .db
        .cards()
        .card_id()
        .filter(card_id)
        .filter(|c| valid_at_time(c.valid_at) > from_time)
        .map(|c| c.valid_at)
        .collect();
    for vat in future {
        let Some(mut row) = ctx.db.cards().valid_at().find(vat) else {
            continue;
        };
        let fv = field_value_as_of(ctx, card_id, aspect, valid_at_time(vat));
        let new_stock = field.set(row.stock, fv);
        if new_stock == row.stock {
            continue; // unchanged — don't churn the row (or its PK).
        }
        ctx.db.cards().valid_at().delete(vat);
        row.stock = new_stock;
        ctx.db.cards().insert(row);
    }
}

/// Record an op AND materialize it onto the card rows in one step — the normal
/// entry point. Append first (so the fold sees it), then re-materialize from the
/// op's `time_ms` forward.
pub fn apply_op(
    ctx: &ReducerContext,
    card_id: u32,
    aspect: StockAspect,
    time_ms: u64,
    op: AspectOp,
    modifier: i64,
) {
    append(ctx, card_id, aspect, time_ms, op, modifier);
    materialize_aspect(ctx, card_id, aspect, time_ms);
}

/// Collapse SETTLED ops (`time_ms <= watermark`) per `(card, aspect)` into a
/// single `Set` checkpoint at the watermark, so a live fold never replays
/// unbounded history. The watermark is `now - max_late_arrival`: nothing older
/// can be reordered against (the server rejects submissions further behind), so
/// the settled prefix is final and folds to a constant.
///
/// Because the op-log is ST-only (no client subscription), this is purely
/// internal — no fan-out, no in-place-upsert dance. A `(card, aspect)` whose
/// settled value folds to 0 (e.g. a hold fully acquired+released) gets its rows
/// deleted with NO checkpoint — released holds leave nothing behind.
pub fn compact(ctx: &ReducerContext, watermark: u64) {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<(u32, u8), Vec<OpLog>> = BTreeMap::new();
    for row in ctx.db.op_log().iter() {
        groups.entry((row.card_id, row.aspect_id)).or_default().push(row);
    }
    for ((card_id, aspect_id), mut rows) in groups {
        let settled: Vec<u64> = rows
            .iter()
            .filter(|r| r.time_ms <= watermark)
            .map(|r| r.id)
            .collect();
        if settled.is_empty() {
            continue; // whole tail is live — nothing to collapse.
        }
        // Already one settled `Set` checkpoint → nothing to do (don't churn it).
        if settled.len() == 1 {
            if let Some(r) = rows.iter().find(|r| r.id == settled[0]) {
                if AspectOp::from_code(r.op) == Some(AspectOp::Set) {
                    continue;
                }
            }
        }
        rows.sort_by_key(|r| r.id);
        let ops: Vec<Op> = rows
            .iter()
            .filter_map(|r| AspectOp::from_code(r.op).map(|op| Op { time: r.time_ms, op, modifier: r.modifier }))
            .collect();
        let value = fold(&ops, watermark); // settled value as-of the watermark
        for id in settled {
            ctx.db.op_log().id().delete(id);
        }
        if value != 0 {
            ctx.db.op_log().insert(OpLog {
                id: 0,
                card_id,
                aspect_id,
                time_ms: watermark,
                op: AspectOp::Set.code(),
                modifier: value,
            });
        }
    }
}

/// `apply_op` for the commutative `±1` refcount path (holds / `dead` / `reap`).
pub fn apply_delta(
    ctx: &ReducerContext,
    card_id: u32,
    aspect: StockAspect,
    time_ms: u64,
    increment: bool,
) {
    let op = if increment { AspectOp::Inc } else { AspectOp::Dec };
    apply_op(ctx, card_id, aspect, time_ms, op, 1);
}
