//! Gate-facing write reducers for the `players` auth/index DB. Authorization
//! is the gateway's job — these trust their arguments.

use spacetimedb::{reducer, ReducerContext};

use crate::players::{set_faction, set_permissions};

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

/// Set `player_id`'s permissions capability byte at `time_ms`. The store is
/// content-agnostic and trusts its args (per the module contract); the gateway
/// authorizes the caller — a player can't grant themselves capabilities. Used
/// to provision content-author / admin accounts (typically in the
/// `0..FIRST_PLAYER_ID` reserved range).
#[reducer]
pub fn set_player_permissions(
    ctx: &ReducerContext,
    player_id: u32,
    time_ms: u64,
    perms: u8,
) -> Result<(), String> {
    set_permissions(ctx, player_id, time_ms, perms)
}
