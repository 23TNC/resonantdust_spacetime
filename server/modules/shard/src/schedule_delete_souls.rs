use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, Timestamp};

use crate::packed::valid_at_time;
use crate::souls::souls;

/// Headroom (in milliseconds) between the cutoff a sweep targets and
/// when the scheduler actually fires it. Same rationale as
/// `schedule_delete_cards::DELETE_DELAY_MS`: shifting the firing time
/// forward gives every connected client time to advance its buffered
/// display clock past the cutoff before the delete event lands. The
/// cutoff itself is unchanged — `delete_souls` derives it back out by
/// subtracting `DELETE_DELAY_MS` from `scheduled_at`, so the
/// strict-less-than rule still preserves the row at
/// `valid_at_time = cutoff`.
const DELETE_DELAY_MS: u64 = 5_000;

#[table(accessor = schedule_delete_souls, scheduled(delete_souls))]
pub struct ScheduleDeleteSouls {
    #[primary_key]
    #[auto_inc]
    pub delete_id: u64,
    #[index(btree)]
    pub card_id: u32,
    pub scheduled_at: ScheduleAt,
}

/// Insert (or coalesce) a one-shot schedule row tied to a soul we
/// just wrote. Mirrors `schedule_delete_cards::enqueue` —
/// `DELETE_DELAY_MS` past the new row's `valid_at`, at most one
/// pending schedule per `card_id` at a time (later one wins when
/// merging). See that module for the full coalesce rationale.
pub fn enqueue(ctx: &ReducerContext, card_id: u32, valid_at: u64) {
    let scheduled_ms = valid_at_time(valid_at).saturating_add(DELETE_DELAY_MS);

    let pending: Vec<(u64, u64)> = ctx
        .db
        .schedule_delete_souls()
        .card_id()
        .filter(card_id)
        .filter_map(|s| match s.scheduled_at {
            ScheduleAt::Time(ts) => Some((
                s.delete_id,
                (ts.to_micros_since_unix_epoch() / 1_000) as u64,
            )),
            ScheduleAt::Interval(_) => None,
        })
        .collect();

    let max_pending = pending.iter().map(|(_, t)| *t).max().unwrap_or(0);
    if !pending.is_empty() && max_pending >= scheduled_ms {
        return;
    }

    for (id, _) in &pending {
        ctx.db.schedule_delete_souls().delete_id().delete(*id);
    }
    let ts = Timestamp::from_micros_since_unix_epoch((scheduled_ms as i64) * 1_000);
    ctx.db.schedule_delete_souls().insert(ScheduleDeleteSouls {
        delete_id: 0,
        card_id,
        scheduled_at: ScheduleAt::Time(ts),
    });
}

#[reducer]
pub fn delete_souls(
    ctx: &ReducerContext,
    args: ScheduleDeleteSouls,
) -> Result<(), String> {
    let scheduled_ms: u64 = match args.scheduled_at {
        ScheduleAt::Time(ts) => (ts.to_micros_since_unix_epoch() / 1_000) as u64,
        ScheduleAt::Interval(_) => return Ok(()),
    };
    let cutoff_ms = scheduled_ms.saturating_sub(DELETE_DELAY_MS);

    let stale: Vec<u64> = ctx
        .db
        .souls()
        .card_id()
        .filter(args.card_id)
        .filter(|s| valid_at_time(s.valid_at) < cutoff_ms)
        .map(|s| s.valid_at)
        .collect();

    for valid_at in stale {
        ctx.db.souls().valid_at().delete(valid_at);
    }

    ctx.db
        .schedule_delete_souls()
        .delete_id()
        .delete(args.delete_id);

    Ok(())
}
