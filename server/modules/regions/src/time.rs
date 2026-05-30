//! Time-discipline helpers, mirrored from the `cards` module's
//! `cards.rs`. The `regions` and `cards` modules are separate crates
//! with no shared runtime dependency, so the client-server drift
//! contract is duplicated here — keep the constants in sync across the
//! two modules.

use spacetimedb::ReducerContext;

/// Wall-clock now in unix milliseconds. The codebase's time unit
/// throughout (`valid_at` rows pack u48 ms).
pub fn now_ms(ctx: &ReducerContext) -> u64 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64
}

/// Client-server time-drift tolerance. The client runs its
/// `serverNowMs()` estimate this far behind the captured server
/// timestamp; used here as the forward-grace ceiling.
pub const TIME_DRIFT_BUFFER_MS: u64 = 2_000;

/// Static backward-grace window. The server accepts `client_time_ms`
/// up to this many ms behind its own clock, rejecting anything older.
pub const BACKWARD_GRACE_MS: u64 = 10_000;

/// Resolve the time to use for game-logic in a reducer: reject if
/// `client_time_ms` is more than `BACKWARD_GRACE_MS` behind or
/// `TIME_DRIFT_BUFFER_MS` ahead of server time, else return
/// `min(client, server)`. Errors use the `time_drift:` prefix so the
/// client can parse the rejection and retry once the gap closes.
///
/// Identical policy to `cards::effective_now_ms` — see that module for
/// the full rationale on why `min` and why the grace windows are sized
/// the way they are.
pub fn effective_now_ms(ctx: &ReducerContext, client_time_ms: u64) -> Result<u64, String> {
    let server = now_ms(ctx);
    let behind = server.saturating_sub(client_time_ms);
    if behind > BACKWARD_GRACE_MS {
        return Err(format!(
            "time_drift:client_behind_by={behind} (server={server}, client={client_time_ms})"
        ));
    }
    let ahead = client_time_ms.saturating_sub(server);
    if ahead > TIME_DRIFT_BUFFER_MS {
        return Err(format!(
            "time_drift:client_ahead_by={ahead} (server={server}, client={client_time_ms})"
        ));
    }
    Ok(client_time_ms.min(server))
}
