use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, Timestamp};

use crate::cards::cards;
use crate::packed::valid_at_time;

/// Headroom (in seconds) between the cutoff a sweep targets and when the
/// scheduler actually fires it. The sweep's strict-less-than rule means
/// firing at exactly `cutoff` would race with clients whose buffered
/// display clock is still slightly behind: the delete event for the
/// previous row could land before the client's `promote(buffered_now)`
/// has had a chance to advance past the new row's `valid_at_time`,
/// causing a brief gap with no current row.
///
/// Shifting the firing time forward by this constant gives every
/// connected client time to advance past the cutoff before the delete
/// event lands. The cutoff itself is unchanged — `delete_cards` derives
/// it back out by subtracting `DELETE_DELAY_SECS` from `scheduled_at`,
/// so the strict-less-than rule still preserves the row at `valid_at_time
/// = cutoff`. Comfortably larger than any reasonable client display
/// buffer (typically 1–3s); tune this against the client's
/// `ValidAtTable.promote(buffered_now)` buffer setting.
const DELETE_DELAY_SECS: u32 = 5;

#[table(accessor = schedule_delete_cards, scheduled(delete_cards))]
pub struct ScheduleDeleteCards {
    #[primary_key]
    #[auto_inc]
    pub delete_id: u64,
    #[index(btree)]
    pub card_id: u32,
    pub scheduled_at: ScheduleAt,
}

// Insert a one-shot schedule row tied to a card we just wrote. The schedule
// fires `DELETE_DELAY_SECS` *after* the new row's valid_at — far enough
// past for connected clients to have advanced their buffered display
// clock past the cutoff. The reducer derives the actual cutoff back out
// of `scheduled_at`, so older rows still get the strict-less-than
// treatment and the just-written row is preserved.
pub fn enqueue(ctx: &ReducerContext, card_id: u32, valid_at: u64) {
    let scheduled_secs = valid_at_time(valid_at).saturating_add(DELETE_DELAY_SECS) as i64;
    let ts = Timestamp::from_micros_since_unix_epoch(scheduled_secs * 1_000_000);
    ctx.db.schedule_delete_cards().insert(ScheduleDeleteCards {
        delete_id: 0,
        card_id,
        scheduled_at: ScheduleAt::Time(ts),
    });
}

#[reducer]
pub fn delete_cards(ctx: &ReducerContext, args: ScheduleDeleteCards) -> Result<(), String> {
    // `enqueue` shifted scheduled_at forward by DELETE_DELAY_SECS to give
    // clients time to advance their buffered_now past the cutoff. Recover
    // the original cutoff by subtracting that delay back out.
    let scheduled_secs = match args.scheduled_at {
        ScheduleAt::Time(ts) => (ts.to_micros_since_unix_epoch() / 1_000_000) as u32,
        // Periodic schedules aren't expected here; bail without doing damage.
        ScheduleAt::Interval(_) => return Ok(()),
    };
    let cutoff_secs = scheduled_secs.saturating_sub(DELETE_DELAY_SECS);

    let stale: Vec<u64> = ctx
        .db
        .cards()
        .card_id()
        .filter(args.card_id)
        .filter(|c| valid_at_time(c.valid_at) < cutoff_secs)
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
