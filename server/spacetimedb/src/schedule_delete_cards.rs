use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, Timestamp};

use crate::cards::cards;
use crate::packed::valid_at_time;

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
// fires at the same wall-clock second as the new row's valid_at, and the
// reducer cleans up any older rows for this card_id (strict-less-than, so the
// row we just wrote is preserved).
pub fn enqueue(ctx: &ReducerContext, card_id: u32, valid_at: u64) {
    let secs = valid_at_time(valid_at) as i64;
    let ts = Timestamp::from_micros_since_unix_epoch(secs * 1_000_000);
    ctx.db.schedule_delete_cards().insert(ScheduleDeleteCards {
        delete_id: 0,
        card_id,
        scheduled_at: ScheduleAt::Time(ts),
    });
}

#[reducer]
pub fn delete_cards(ctx: &ReducerContext, args: ScheduleDeleteCards) -> Result<(), String> {
    let cutoff_secs = match args.scheduled_at {
        ScheduleAt::Time(ts) => (ts.to_micros_since_unix_epoch() / 1_000_000) as u32,
        // Periodic schedules aren't expected here; bail without doing damage.
        ScheduleAt::Interval(_) => return Ok(()),
    };

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
