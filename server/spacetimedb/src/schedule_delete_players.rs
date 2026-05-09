use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, Timestamp};

use crate::packed::valid_at_time;
use crate::players::players;

/// Headroom between the cutoff a sweep targets and when the scheduler
/// fires it. See `schedule_delete_cards::DELETE_DELAY_SECS` for the full
/// rationale — same trade-off applies to player history. Kept in sync
/// with the cards-side constant; both should be set the same value.
const DELETE_DELAY_SECS: u32 = 5;

#[table(accessor = schedule_delete_players, scheduled(delete_players))]
pub struct ScheduleDeletePlayers {
    #[primary_key]
    #[auto_inc]
    pub delete_id: u64,
    #[index(btree)]
    pub player_id: u32,
    pub scheduled_at: ScheduleAt,
}

// Insert a one-shot schedule row tied to a player we just wrote. Mirrors
// `schedule_delete_cards::enqueue` — shifts scheduled_at forward by
// `DELETE_DELAY_SECS` so connected clients have time to advance past the
// cutoff before the delete event arrives.
pub fn enqueue(ctx: &ReducerContext, player_id: u32, valid_at: u64) {
    let scheduled_secs = valid_at_time(valid_at).saturating_add(DELETE_DELAY_SECS) as i64;
    let ts = Timestamp::from_micros_since_unix_epoch(scheduled_secs * 1_000_000);
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
    // Recover the original cutoff by undoing the DELETE_DELAY_SECS shift
    // applied at enqueue time.
    let scheduled_secs = match args.scheduled_at {
        ScheduleAt::Time(ts) => (ts.to_micros_since_unix_epoch() / 1_000_000) as u32,
        ScheduleAt::Interval(_) => return Ok(()),
    };
    let cutoff_secs = scheduled_secs.saturating_sub(DELETE_DELAY_SECS);

    let stale: Vec<u64> = ctx
        .db
        .players()
        .player_id()
        .filter(args.player_id)
        .filter(|p| valid_at_time(p.valid_at) < cutoff_secs)
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
