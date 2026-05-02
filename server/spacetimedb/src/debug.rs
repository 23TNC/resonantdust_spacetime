//! Debug-only reducers for development. These bypass the normal game flow
//! (no authentication, no recipe checks) and are gated behind the `debug`
//! cargo feature so they aren't compiled into production builds. The
//! validation that `cards::insert_card_row` performs (layer check,
//! target-player existence) and that `definitions::find_packed` performs
//! (real card name) still applies.

use spacetimedb::{reducer, ReducerContext};

use crate::cards::{insert_card_row, LAYER_INVENTORY};
use crate::definitions::find_packed;

/// Spawn a card directly into a player's inventory. Skips authentication
/// and recipe checks. The new `card_id` is auto-assigned; clients learn it
/// via subscription.
///
/// `card_path` identifies the card by `"type/key"` (category defaults to
/// `"default"`) or `"type/category/key"`. The path is resolved against
/// `data/card_types.json` and `data/cards/*.json` via
/// `definitions::find_packed`, so a typo or unregistered card name returns
/// a descriptive error rather than spawning a bogus row.
///
/// Real card-creation reducers will replace this once recipe-driven card
/// creation lands.
#[reducer]
pub fn debug_spawn(
  ctx: &ReducerContext,
  player_id: u32,
  card_path: String,
) -> Result<(), String> {
  let packed_definition = find_packed(&card_path)?;
  // Debug spawns put the card into `player_id`'s inventory and assign that
  // same player as the owner. Real card-creation reducers will set these
  // independently as appropriate.
  insert_card_row(ctx, LAYER_INVENTORY, player_id, player_id, packed_definition)?;
  Ok(())
}
