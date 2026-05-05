//! Scheduled-reducer lag, surfaced to clients on public tables.
//!
//! Public-table rows (`Card`, `Action`, `MagneticAction`, `Player`,
//! `Zone`) carry a `delta_t: u8` column. Each unit is **16 ms**. The
//! value is the gap between when a scheduled reducer was supposed to
//! fire and when it actually ran — so the client can back-date its
//! animations by `16 * delta_t` ms instead of treating the row update
//! as "happening now."
//!
//! Default `0` for client-driven writes (the call stack is outside
//! any scheduled reducer). Scheduled reducers install a [`Guard`] at
//! their entry point via [`enter`]; every public-table write inside
//! that scope reads the guard's value via [`current`] and stamps the
//! field. The guard restores the previous value on drop, so nested
//! scheduled fires (which SpacetimeDB doesn't currently produce, but
//! defensively) round-trip correctly.
//!
//! Saturating semantics: lag larger than `u8::MAX * 16 ms` (~4.08 s)
//! clamps. A clamp event means the client-side compensation buffer
//! was overrun anyway; logging it is the client's job.

use std::cell::Cell;

thread_local! {
  static CURRENT: Cell<u8> = const { Cell::new(0) };
}

/// Current scheduled-reducer lag in 16-ms units. `0` outside any
/// [`Guard`] scope, which is the right answer for client-driven
/// writes.
pub fn current() -> u8 {
  CURRENT.with(|c| c.get())
}

/// Install a lag value for the duration of the returned [`Guard`].
/// Drops restore the previous value, so the call stack always
/// observes a coherent reading. Top-of-scheduled-reducer use:
///
/// ```ignore
/// let scheduled_micros = scheduler.scheduled_at_time_micros();
/// let now_micros = ctx.timestamp.to_micros_since_unix_epoch();
/// let _guard = delta_t::enter(delta_t::compute(scheduled_micros, now_micros));
/// ```
#[must_use = "Guard restores the previous value when dropped; binding it to `_` drops it instantly"]
pub fn enter(value: u8) -> Guard {
  let prev = CURRENT.with(|c| c.replace(value));
  Guard { prev }
}

pub struct Guard {
  prev: u8,
}

impl Drop for Guard {
  fn drop(&mut self) {
    CURRENT.with(|c| c.set(self.prev));
  }
}

/// Lag in 16-ms units, rounded to nearest, saturating at `u8::MAX`.
/// `scheduled_micros` is when the reducer was supposed to fire;
/// `now_micros` is `ctx.timestamp.to_micros_since_unix_epoch()`.
/// Negative or zero deltas (early/exact) return `0`.
pub fn compute(scheduled_micros: i64, now_micros: i64) -> u8 {
  let delta_us = now_micros.saturating_sub(scheduled_micros);
  if delta_us <= 0 {
    return 0;
  }
  let delta_ms = delta_us / 1_000;
  // Round to nearest 16-ms step.
  let steps = (delta_ms + 8) / 16;
  if steps > u8::MAX as i64 {
    u8::MAX
  } else {
    steps as u8
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn compute_zero_when_early_or_exact() {
    assert_eq!(compute(1_000_000, 1_000_000), 0);
    assert_eq!(compute(2_000_000, 1_000_000), 0);
  }

  #[test]
  fn compute_rounds_to_nearest_16ms() {
    // 8 ms late -> rounds to 1 step
    assert_eq!(compute(0, 8_000), 1);
    // 7 ms late -> rounds to 0 steps
    assert_eq!(compute(0, 7_000), 0);
    // 16 ms late -> 1 step
    assert_eq!(compute(0, 16_000), 1);
    // 24 ms late -> rounds to 2 steps
    assert_eq!(compute(0, 24_000), 2);
  }

  #[test]
  fn compute_saturates_at_u8_max() {
    // Far past saturating threshold (~4.08 s)
    assert_eq!(compute(0, 10_000_000), u8::MAX);
  }

  #[test]
  fn enter_nests_and_restores() {
    assert_eq!(current(), 0);
    {
      let _outer = enter(5);
      assert_eq!(current(), 5);
      {
        let _inner = enter(20);
        assert_eq!(current(), 20);
      }
      assert_eq!(current(), 5);
    }
    assert_eq!(current(), 0);
  }
}
