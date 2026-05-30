//! Gate-facing write reducers for the `players` auth/index DB. Authorization
//! is the gateway's job — these trust their arguments.

use spacetimedb::{reducer, ReducerContext};

use crate::players::set_faction;

/// Set `player_id`'s faction at `time_ms` — the public reducer the `players`
/// module anticipated for the recipe `…aspect.faction.set` effect, now
/// driven by the gateway instead of an in-module action path.
#[reducer]
pub fn set_player_faction(
    ctx: &ReducerContext,
    player_id: u32,
    time_ms: u64,
    faction: u8,
) -> Result<(), String> {
    set_faction(ctx, player_id, time_ms, faction)
}
