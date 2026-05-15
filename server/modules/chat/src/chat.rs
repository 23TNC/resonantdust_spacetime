use spacetimedb::{reducer, table, ReducerContext, ScheduleAt, Table, TimeDuration};

use crate::packed::{pack_valid_at, valid_at_time};
use crate::sequence;

/// Reserved player_id used as a placeholder for system / unauthenticated
/// senders. Mirrors `WORLD_PLAYER_ID` in the shard module — same value,
/// same intent ("no real player owns this"). Hard-coded here rather than
/// imported so the chat module stays free of gameplay-side dependencies.
const ANONYMOUS_PLAYER_ID: u32 = 0;

/// Maximum byte length of a sender display name. Mirrors
/// `players::MAX_PLAYER_NAME_LEN` in the shard module — same bound,
/// re-declared locally so the chat module doesn't reach into shard.
const MAX_SENDER_NAME_LEN: usize = 64;

/// Maximum byte length of a chat `body`. 1 KiB is generous for text chat
/// while keeping a single message well under any practical row size.
const MAX_BODY_LEN: usize = 1024;

/// Retention window. Messages older than this are deleted by the
/// recurring sweep below. Tuned for "you missed up to one hour while
/// you were away" — clients subscribe with a threshold derived from
/// their previous `last_login_secs`, capped against this retention,
/// so the practical scrollback ceiling is whichever bound is tighter.
const RETENTION_MS: u64 = 60 * 60 * 1_000;

/// How often the retention sweep fires. Shorter = less drift between
/// the policy and what's actually in the table, but more reducer
/// invocations. One minute is well below the retention window and
/// adds negligible load.
const SWEEP_INTERVAL_MS: i64 = 60_000;

/// World-chat message. Append-only — there's no edit/delete path, no
/// version history. Sender names are **denormalised** onto the row so
/// clients can render `name: body` without joining against any other
/// table — the chat module doesn't have access to one anyway, since it
/// owns no gameplay state. Rename isn't a feature today and there's no
/// plan to retroactively rewrite history if it ever becomes one — old
/// messages stay attributed to whatever the sender was called when they
/// sent.
///
/// Public. Clients subscribe with `SELECT * FROM chat_messages` (no
/// channel scoping — `general` is the only feed today; if a future
/// `local` lands, see the chat design doc for the two options).
#[spacetimedb::table(accessor = chat_messages, public)]
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Packed `[time_ms: u48 | seq: u16]` (high | low). Same shape as
    /// the shard module's `valid_at` — `sequence::next_sequence` fills
    /// the low 16 bits so two same-millisecond writes don't collide on
    /// the PK. Sorting by `sent_at` gives chronological order in one
    /// pass.
    #[primary_key]
    pub sent_at: u64,
    /// `player_id` supplied by the caller. With no `players` table in
    /// this module to validate against, the chat module trusts the
    /// caller — eventually a sidecar (or chat-side mirror of
    /// `player_sessions`) is the trust boundary that prevents
    /// spoofing. Kept on the row for filtering / future moderation
    /// tooling even though `sender_name` is what the UI renders.
    #[index(btree)]
    pub sender_player_id: u32,
    /// Sender's display name as supplied by the caller. Frozen for the
    /// row's lifetime. See the table doc for the rename-history policy.
    pub sender_name: String,
    /// Message body, trimmed and validated server-side.
    pub body: String,
}

/// Recurring schedule for the retention sweep. Single-row table: one
/// `ScheduleAt::Interval` row is seeded by the `init` reducer and
/// re-fires every `SWEEP_INTERVAL_MS`. The arg row passed to
/// `chat_retention_sweep` is the schedule row itself (SpacetimeDB
/// convention for `scheduled(...)` tables).
#[table(accessor = chat_retention, scheduled(chat_retention_sweep))]
pub struct ChatRetention {
    #[primary_key]
    #[auto_inc]
    pub id: u64,
    pub scheduled_at: ScheduleAt,
}

/// Module init — runs once on fresh publish. Seeds the recurring
/// retention sweep. Idempotent: re-publishing without
/// `--delete-data` doesn't re-run `init`, but if for any reason
/// the schedule row vanishes (manual `spacetime sql DELETE`,
/// schema migration), the sweep stops firing — which is recoverable
/// by re-publishing with `--delete-data` or manually re-inserting.
#[reducer(init)]
pub fn init(ctx: &ReducerContext) {
    // Defensive: skip if a row already exists. `auto_inc` would happily
    // insert a duplicate, leaving two sweeps firing in parallel.
    if ctx.db.chat_retention().iter().next().is_some() {
        return;
    }
    ctx.db.chat_retention().insert(ChatRetention {
        id: 0,
        scheduled_at: ScheduleAt::Interval(TimeDuration::from_micros(
            SWEEP_INTERVAL_MS.saturating_mul(1_000),
        )),
    });
}

/// Send a chat message to the world feed. The caller supplies the
/// sender's `player_id` and display `name` explicitly — the chat
/// module owns no `players` table to look them up. Validates the body
/// (length, no control chars) and the supplied name (length, no
/// control chars), then writes a new `ChatMessage` row whose `sent_at`
/// is the packed (time, seq) key — clients see it via their
/// `chat_messages` subscription.
///
/// **Trust.** With no session table in this module, the chat module
/// can't independently verify that `sender_player_id` matches
/// `ctx.sender()`. Clients calling this directly could spoof either
/// field. The intended deployment is sidecar-mediated:
/// `claim_or_login` happens against the shard / identity database;
/// the sidecar resolves the caller, then invokes this reducer with
/// the resolved `(player_id, name)`. Until that's wired up, treat
/// this as a development-only entry point.
#[reducer]
pub fn send_chat_message(
    ctx: &ReducerContext,
    sender_player_id: u32,
    sender_name: String,
    body: String,
) -> Result<(), String> {
    let trimmed_body = validate_body(&body)?;
    let trimmed_name = validate_sender_name(&sender_name, sender_player_id)?;

    let time_ms = (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64;
    let sent_at = pack_valid_at(time_ms, sequence::next_sequence(ctx));
    ctx.db.chat_messages().insert(ChatMessage {
        sent_at,
        sender_player_id,
        sender_name: trimmed_name,
        body: trimmed_body,
    });
    Ok(())
}

/// Walk `chat_messages` and delete rows older than `RETENTION_MS`.
/// Fired on the recurring schedule seeded by `init`.
///
/// O(N) over the table — acceptable while N is bounded by retention.
/// Once the table has accumulated a sweep-worth of messages, every
/// subsequent sweep evicts roughly the per-window write volume, so
/// table size stabilises near `(send rate) * RETENTION_MS`.
#[reducer]
pub fn chat_retention_sweep(
    ctx: &ReducerContext,
    _row: ChatRetention,
) -> Result<(), String> {
    let now_ms = (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64;
    let cutoff_ms = now_ms.saturating_sub(RETENTION_MS);

    let stale: Vec<u64> = ctx
        .db
        .chat_messages()
        .iter()
        .filter(|m| valid_at_time(m.sent_at) < cutoff_ms)
        .map(|m| m.sent_at)
        .collect();
    for sent_at in stale {
        ctx.db.chat_messages().sent_at().delete(sent_at);
    }
    Ok(())
}

/// Trim, then enforce: non-empty, length cap, no control characters
/// other than newline. Returns the cleaned body. Same shape as
/// `validate_sender_name` (and as the shard module's
/// `players::validate_player_name`).
fn validate_body(body: &str) -> Result<String, String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err("chat message cannot be empty".to_string());
    }
    if trimmed.len() > MAX_BODY_LEN {
        return Err(format!(
            "chat message length {} exceeds max {}",
            trimmed.len(),
            MAX_BODY_LEN,
        ));
    }
    if trimmed.chars().any(|c| c.is_control() && c != '\n') {
        return Err("chat message cannot contain control characters".to_string());
    }
    Ok(trimmed.to_string())
}

/// Validate a sender display name supplied by the caller. Same rules
/// as `validate_body` minus the newline allowance (names are
/// single-line). Empty / control-char names from anonymous senders
/// are rebranded to `p:<id>` rather than rejected, so a sidecar that
/// passes through `(0, "")` for system messages still gets a useful
/// placeholder rather than an error. Non-empty user names are
/// strictly validated.
fn validate_sender_name(name: &str, player_id: u32) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        if player_id == ANONYMOUS_PLAYER_ID {
            return Ok("system".to_string());
        }
        return Ok(format!("p:{player_id}"));
    }
    if trimmed.len() > MAX_SENDER_NAME_LEN {
        return Err(format!(
            "sender name length {} exceeds max {}",
            trimmed.len(),
            MAX_SENDER_NAME_LEN,
        ));
    }
    if trimmed.chars().any(|c| c.is_control()) {
        return Err("sender name cannot contain control characters".to_string());
    }
    Ok(trimmed.to_string())
}
