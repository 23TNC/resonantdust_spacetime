use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table};
use std::time::Duration;

#[table(accessor = tick_schedule, scheduled(tick))]
pub struct TickSchedule {
  #[primary_key]
  #[auto_inc]
  pub id: u64,

  pub scheduled_at: ScheduleAt,
}

#[reducer]
pub fn tick(ctx: &ReducerContext, _tick: TickSchedule) -> Result<(), String> {
  // process due actions / recipes here
  Ok(())
}

#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
  ctx.db.tick_schedule().insert(TickSchedule {
    id: 0,
    scheduled_at: ScheduleAt::Interval(Duration::from_secs(5).into()),
  });
}