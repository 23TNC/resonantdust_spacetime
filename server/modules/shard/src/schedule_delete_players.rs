use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, Timestamp};

use crate::packed::valid_at_time;
use crate::players::players;

/// Headroom between the cutoff a sweep targets and when the scheduler
/// fires it. See `schedule_delete_cards::DELETE_DELAY_MS` for the full
/// rationale — same trade-off applies to player history. Kept in sync
/// with the cards-side constant; both should be set the same value.
const DELETE_DELAY_MS: u64 = 5_000;

#[table(accessor = schedule_delete_players, scheduled(delete_players))]
pub struct ScheduleDeletePlayers {
    #[primary_key]
    #[auto_inc]
    pub delete_id: u64,
    #[index(btree)]
    pub player_id: u32,
    pub scheduled_at: ScheduleAt,
}

// Insert (or coalesce) a one-shot schedule row tied to a player we
// just wrote. Mirrors `schedule_delete_cards::enqueue` — see that
// file's comment block for the full coalescing rationale. At most
// one pending schedule per `player_id` at any time; rapidly-written
// players enqueue one sweep instead of N.
pub fn enqueue(ctx: &ReducerContext, player_id: u32, valid_at: u64) {
    let scheduled_ms = valid_at_time(valid_at).saturating_add(DELETE_DELAY_MS);

    let pending: Vec<(u64, u64)> = ctx
        .db
        .schedule_delete_players()
        .player_id()
        .filter(player_id)
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
        ctx.db.schedule_delete_players().delete_id().delete(*id);
    }
    let ts = Timestamp::from_micros_since_unix_epoch((scheduled_ms as i64) * 1_000);
    ctx.db
        .schedule_delete_players()
        .insert(ScheduleDeletePlayers {
            delete_id: 0,
            player_id,
            scheduled_at: ScheduleAt::Time(ts),
        });
}

#[reducer]
pub fn delete_players(
    ctx: &ReducerContext,
    args: ScheduleDeletePlayers,
) -> Result<(), String> {
    // Recover the original cutoff by undoing the DELETE_DELAY_MS shift
    // applied at enqueue time.
    let scheduled_ms: u64 = match args.scheduled_at {
        ScheduleAt::Time(ts) => (ts.to_micros_since_unix_epoch() / 1_000) as u64,
        ScheduleAt::Interval(_) => return Ok(()),
    };
    let cutoff_ms = scheduled_ms.saturating_sub(DELETE_DELAY_MS);

    let stale: Vec<u64> = ctx
        .db
        .players()
        .player_id()
        .filter(args.player_id)
        .filter(|p| valid_at_time(p.valid_at) < cutoff_ms)
        .map(|p| p.valid_at)
        .collect();

    for valid_at in stale {
        ctx.db.players().valid_at().delete(valid_at);
    }

    ctx.db
        .schedule_delete_players()
        .delete_id()
        .delete(args.delete_id);

    Ok(())
}
