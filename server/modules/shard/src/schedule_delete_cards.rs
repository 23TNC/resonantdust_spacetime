use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, Timestamp};

use crate::cards::cards;
use crate::packed::valid_at_time;

/// Headroom (in milliseconds) between the cutoff a sweep targets and
/// when the scheduler actually fires it. The sweep's strict-less-than
/// rule means firing at exactly `cutoff` would race with clients whose
/// buffered display clock is still slightly behind: the delete event
/// for the previous row could land before the client's
/// `promote(buffered_now)` has had a chance to advance past the new
/// row's `valid_at_time`, causing a brief gap with no current row.
///
/// Shifting the firing time forward by this constant gives every
/// connected client time to advance past the cutoff before the delete
/// event lands. The cutoff itself is unchanged — `delete_cards` derives
/// it back out by subtracting `DELETE_DELAY_MS` from `scheduled_at`,
/// so the strict-less-than rule still preserves the row at
/// `valid_at_time = cutoff`. Comfortably larger than any reasonable
/// client display buffer (typically 1–3s); tune this against the
/// client's `ValidAtTable.promote(buffered_now)` buffer setting.
const DELETE_DELAY_MS: u64 = 5_000;

#[table(accessor = schedule_delete_cards, scheduled(delete_cards))]
pub struct ScheduleDeleteCards {
    #[primary_key]
    #[auto_inc]
    pub delete_id: u64,
    #[index(btree)]
    pub card_id: u32,
    pub scheduled_at: ScheduleAt,
}

// Insert (or coalesce) a one-shot schedule row tied to a card we just
// wrote. The schedule fires `DELETE_DELAY_MS` *after* the new row's
// valid_at — far enough past for connected clients to have advanced
// their buffered display clock past the cutoff. The reducer derives
// the actual cutoff back out of `scheduled_at`, so older rows still
// get the strict-less-than treatment and the just-written row is
// preserved.
//
// **Coalescing.** At most one pending schedule per `card_id` at any
// time. Each call:
//   1. Looks up the pending schedule(s) for this card via the
//      `card_id` btree index.
//   2. If any has `scheduled_at >= ours`, leave them — their later
//      cutoff is a superset of ours (their sweep will delete every
//      row older than its cutoff, which includes anything older than
//      our `valid_at`), and their later fire-time still preserves
//      our new row (it's > their cutoff, so safe).
//   3. Otherwise delete every existing pending and insert a single
//      new schedule carrying our (later) cutoff.
//
// Saves a schedule-fire-per-write. A busy card written 30 times in a
// short window enqueues 1 sweep instead of 30. The defensive
// "multiple existing" branch handles the transitional case during
// rollout — once enforced, there's at most one per card_id.
pub fn enqueue(ctx: &ReducerContext, card_id: u32, valid_at: u64) {
    let scheduled_ms = (valid_at_time(valid_at)).saturating_add(DELETE_DELAY_MS);

    let pending: Vec<(u64, u64)> = ctx
        .db
        .schedule_delete_cards()
        .card_id()
        .filter(card_id)
        .filter_map(|s| match s.scheduled_at {
            ScheduleAt::Time(ts) => Some((
                s.delete_id,
                (ts.to_micros_since_unix_epoch() / 1_000) as u64,
            )),
            // Periodic schedules aren't expected here; skip them.
            ScheduleAt::Interval(_) => None,
        })
        .collect();

    let max_pending = pending.iter().map(|(_, t)| *t).max().unwrap_or(0);
    if !pending.is_empty() && max_pending >= scheduled_ms {
        return;
    }

    for (id, _) in &pending {
        ctx.db.schedule_delete_cards().delete_id().delete(*id);
    }
    let ts = Timestamp::from_micros_since_unix_epoch((scheduled_ms as i64) * 1_000);
    ctx.db.schedule_delete_cards().insert(ScheduleDeleteCards {
        delete_id: 0,
        card_id,
        scheduled_at: ScheduleAt::Time(ts),
    });
}

#[reducer]
pub fn delete_cards(ctx: &ReducerContext, args: ScheduleDeleteCards) -> Result<(), String> {
    // `enqueue` shifted scheduled_at forward by DELETE_DELAY_MS to give
    // clients time to advance their buffered_now past the cutoff. Recover
    // the original cutoff by subtracting that delay back out.
    let scheduled_ms: u64 = match args.scheduled_at {
        ScheduleAt::Time(ts) => (ts.to_micros_since_unix_epoch() / 1_000) as u64,
        // Periodic schedules aren't expected here; bail without doing damage.
        ScheduleAt::Interval(_) => return Ok(()),
    };
    let cutoff_ms = scheduled_ms.saturating_sub(DELETE_DELAY_MS);

    let stale: Vec<u64> = ctx
        .db
        .cards()
        .card_id()
        .filter(args.card_id)
        .filter(|c| valid_at_time(c.valid_at) < cutoff_ms)
        .map(|c| c.valid_at)
        .collect();

    for valid_at in stale {
        ctx.db.cards().valid_at().delete(valid_at);
    }

    // One-shot schedule — clear our own row so the table doesn't accumulate.
    ctx.db
        .schedule_delete_cards()
        .delete_id()
        .delete(args.delete_id);

    Ok(())
}
