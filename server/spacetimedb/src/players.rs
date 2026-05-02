use spacetimedb::{reducer, Identity, ReducerContext, Table};
use std::collections::BTreeSet;

// Brings the `cards` table-accessor trait into scope so `ctx.db.cards()`
// resolves here (used by `delete_player` to cascade card cleanup).
use crate::cards::cards as _;

/// Maximum byte length of a `Player.name`. Enforced by `validate_player_name`
/// on the input name and again after normalization in `claim_or_login`.
pub const MAX_PLAYER_NAME_LEN: usize = 64;

#[spacetimedb::table(accessor = players, public)]
#[derive(Debug, Clone)]
pub struct Player {
  #[primary_key]
  #[auto_inc]
  pub player_id: u32,
  /// Display name. Casing preserved for rendering.
  pub name: String,
  /// Lowercased version of `name` used for case-insensitive uniqueness. Must
  /// always equal `normalize_player_name(&name)`. The registration reducer
  /// is responsible for setting this.
  #[unique]
  pub name_normalized: String,
  /// World layer the player's soul currently occupies. `0` while the soul
  /// is not yet placed in the world (the state every player starts in
  /// today, since the world board is not yet implemented). Once world
  /// layers land, expect values in `64..=254`.
  pub layer: u8,
  /// World macro_zone the soul currently occupies. `0` while unplaced.
  #[index(btree)]
  pub macro_zone: u32,
  /// In-zone hex position of the soul. `0` while unplaced.
  pub micro_zone: u8,
  /// Within-`micro_zone` position of the soul. Parallel to
  /// `Card.micro_location`: variant per the soul's stack state — either a
  /// parent `card_id` (if the soul is attached to another card) or packed
  /// `(i16 x, i16 y)` pixel coords (if loose). `0` while unplaced.
  pub micro_location: u32,
}

/// Maps a connection's current `Identity` to the persistent `player_id`.
///
/// `Identity` is treated as ephemeral — a player who reconnects (or signs in
/// fresh) generally arrives with a new `Identity`. A login reducer (not yet
/// written) creates a row here once the caller has authenticated against
/// their persistent identifier (`Player.name_normalized`); the
/// `client_disconnected` lifecycle reducer below removes it on disconnect.
/// Regular reducers go through `resolve_caller` to map `ctx.sender` to the
/// stable `player_id`.
///
/// Private — clients have no need to subscribe.
#[spacetimedb::table(accessor = player_sessions)]
#[derive(Debug, Clone)]
pub struct PlayerSession {
  #[primary_key]
  pub identity: Identity,
  #[index(btree)]
  pub player_id: u32,
}

/// Canonicalize a player name for case-insensitive uniqueness lookups.
/// Whatever this returns is what gets stored in `Player.name_normalized`.
pub fn normalize_player_name(name: &str) -> String {
  name.to_lowercase()
}

/// Validate a name before inserting it into a `Player` row.
///
/// Length is checked in bytes (not chars) since storage is what we're bounding.
/// Whitespace-only and control-character names are rejected so that no player
/// can render as blank or smuggle in characters that break logging / display.
pub fn validate_player_name(name: &str) -> Result<(), String> {
  if name.is_empty() {
    return Err("player name cannot be empty".to_string());
  }
  if name.trim().is_empty() {
    return Err("player name cannot be only whitespace".to_string());
  }
  if name.chars().any(|c| c.is_control()) {
    return Err("player name cannot contain control characters".to_string());
  }
  if name.len() > MAX_PLAYER_NAME_LEN {
    return Err(format!(
      "player name length {} exceeds max {}",
      name.len(),
      MAX_PLAYER_NAME_LEN,
    ));
  }
  Ok(())
}

/// Resolve the calling identity to a `player_id`. Returns `Err` if this
/// connection has not yet authenticated.
///
/// Invariant assumed but not verified here: any `PlayerSession.player_id`
/// references an existing `Player` row. Maintained by routing every player
/// deletion through `delete_player` so dangling sessions can't outlive their
/// player. A returned `player_id` is therefore trusted by callers without a
/// follow-up `players()` lookup.
pub fn resolve_caller(ctx: &ReducerContext) -> Result<u32, String> {
  ctx
    .db
    .player_sessions()
    .identity()
    .find(ctx.sender())
    .map(|s| s.player_id)
    .ok_or_else(|| "caller has no active session".to_string())
}

/// Delete a `Player` row and cascade-clean every `PlayerSession` and
/// `Card` that references it. Anything that wants to remove a player must
/// go through this helper — deleting the row directly would leave dangling
/// sessions (breaking `resolve_caller`'s invariant) and orphan cards
/// (rows whose `macro_zone` or `owner_id` point at a vanished player).
///
/// Cards are collected from two indexes:
/// - `Card.macro_zone == player_id` — cards sitting in the deleted
///   player's inventory regardless of who owns them.
/// - `Card.owner_id == player_id` — cards the deleted player owns,
///   regardless of which inventory they're sitting in.
///
/// Same card may match both queries (it's both stashed in this player's
/// inventory and owned by them), so card_ids are deduped through a
/// `BTreeSet` before deletion.
///
/// TODO: when the Action table lands, also cancel any actions owned by
/// this player so we don't leave orphan in-progress recipes.
pub fn delete_player(ctx: &ReducerContext, player_id: u32) {
  // Collect session identities first, then delete — avoids any
  // iterator-vs-mutation hazards inside SpacetimeDB's table handles.
  let session_ids: Vec<Identity> = ctx
    .db
    .player_sessions()
    .player_id()
    .filter(&player_id)
    .map(|s| s.identity)
    .collect();
  for identity in session_ids {
    ctx.db.player_sessions().identity().delete(&identity);
  }

  // Both indexes are btree, so the per-side filters are O(matches).
  let mut card_ids: BTreeSet<u32> = BTreeSet::new();
  for c in ctx.db.cards().owner_id().filter(&player_id) {
    card_ids.insert(c.card_id);
  }
  for c in ctx.db.cards().macro_zone().filter(&player_id) {
    card_ids.insert(c.card_id);
  }
  for card_id in card_ids {
    ctx.db.cards().card_id().delete(&card_id);
  }

  ctx.db.players().player_id().delete(&player_id);
}

/// Trust-on-first-use registration / login.
///
/// If no `Player` exists with the given normalized name, one is created.
/// Either way, a `PlayerSession` is established (or replaced) for the
/// caller's current `Identity`, mapping it to that `Player.player_id`.
///
/// **This is intentionally insecure.** Anyone can call `claim_or_login`
/// with any name and become that player — there is no password, token, or
/// external auth check. The first connection to claim a name owns it; any
/// later connection that calls `claim_or_login` with the same name will
/// also be granted a session. Replace this with token-based or external
/// auth before exposing the module to anyone you don't trust.
///
/// On a successful login the existing `Player.name` (display casing) is
/// **not** updated — only the original registrar's casing is preserved, so
/// later callers can't change a player's displayed name by logging in with
/// different capitalization.
#[reducer]
pub fn claim_or_login(ctx: &ReducerContext, name: String) -> Result<(), String> {
  validate_player_name(&name)?;
  let normalized = normalize_player_name(&name);
  // `to_lowercase` can in some Unicode cases produce more bytes than the
  // input (e.g. `İ` → `i̇`). Re-check the post-normalize length so a name
  // that passes the input bound can't sneak through into storage.
  if normalized.len() > MAX_PLAYER_NAME_LEN {
    return Err(format!(
      "normalized player name length {} exceeds max {}",
      normalized.len(),
      MAX_PLAYER_NAME_LEN,
    ));
  }

  let player_id = match ctx.db.players().name_normalized().find(&normalized) {
    Some(player) => player.player_id,
    None => {
      let row = ctx.db.players().insert(Player {
        player_id: 0,
        name,
        name_normalized: normalized,
        layer: 0,
        macro_zone: 0,
        micro_zone: 0,
        micro_location: 0,
      });
      row.player_id
    }
  };

  // Replace any prior session on this connection. Delete is idempotent, so
  // if there isn't one (the common case on a fresh connect) this is a
  // no-op. The subsequent insert can then never collide.
  let sender = ctx.sender();
  ctx.db.player_sessions().identity().delete(&sender);
  ctx.db.player_sessions().insert(PlayerSession {
    identity: sender,
    player_id,
  });

  Ok(())
}

/// Clean up the disconnecting connection's `PlayerSession` row.
///
/// SpacetimeDB calls this automatically on every client disconnect. Delete
/// is idempotent — if the connection never logged in (no `PlayerSession`
/// row existed), this is a harmless no-op.
///
/// TODO: when the Action table lands, this should also pause or cancel any
/// in-progress actions owned by the disconnecting player.
#[reducer(client_disconnected)]
pub fn client_disconnected(ctx: &ReducerContext) {
  let sender = ctx.sender();
  ctx.db.player_sessions().identity().delete(&sender);
}
