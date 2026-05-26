//! Character creation.
//!
//! A "character" here is a soul card the player controls, plus the
//! starting inventory of cards that came with it. The single reducer,
//! [`create_character`], picks both from a starter pack — the pack
//! names the soul species (`pack.soul`) and lists the cards-and-
//! counts to spawn into the new soul's inventory (`pack.contents`).
//! Pack data lives in [`resonantdust_content::starter_pack_core`],
//! sourced from `content/starter_packs/data/**/*.json` with stable
//! u16 ids declared in `content/starter_packs/id.json`. The reducer
//! takes only the stable id; everything else (soul species, card
//! list, counts) is registry-driven so JSON edits don't require
//! server-code changes.
//!
//! # Eligibility
//!
//! - **Unlock bit.** The pack's bit in `PlayerProfile.starter_packs`
//!   (position `id - 1`) must be set. Fresh players land with
//!   `starter_packs = 1` (bit 0 → pack id 1, the human default
//!   pack); future packs become available via separate unlock
//!   reducers.
//! - **Soul cap.** A player may own at most `soul_info.max` alive
//!   souls at once (default `5`, configured per-profile and packed
//!   into the low nibble of `PlayerProfile.soul_info`). The current
//!   count lives in the high nibble of the same byte, maintained
//!   delta-style by `crate::souls::on_card_write`, so the gate is a
//!   single row read — no `cards` scan.
//!
//! # Side effects (success path)
//!
//! 1. A new soul card lands at world origin: `surface = WORLD_LAYER`,
//!    `macro_zone = (0, 0)`, `micro_zone = Free(0, 0)`,
//!    `owner_id = caller's player_id`, `flags = FLAG_OWNED_BY_PLAYER`.
//!    The soul is `Free` at the world coords — under the unified card
//!    model there's no separate "on hex" state; world placement *is*
//!    Free with `(q, r)` encoded in micro_zone.
//! 2. For each `StarterPackItem`, `count` copies are spawned in the
//!    soul's inventory: `surface = INVENTORY_LAYER`,
//!    `macro_zone = owner_id = soul.card_id`, `micro_zone = 0`,
//!    `flags = 0`.
//! 3. OnCreate recipe matching is now client-driven — the client scans
//!    root-only recipes against each newly-created card and submits a
//!    `propose_action` if any apply.
//!
//! The unlock bit is **not** cleared on character creation — a pack
//! stays usable as long as it's unlocked and the player has room
//! under the soul cap. That's what supports "delete a character and
//! roll another from the same pack."

use spacetimedb::{reducer, ReducerContext, Table};

use resonantdust_content::definition_core::find_packed_by_key;
use resonantdust_content::starter_pack_core::{
    starter_blueprints_for_soul, starter_pack, StarterPackId,
};

use crate::cards::{self, cards as _cards_table};
use crate::flags::state_flags;
use crate::packed::{nibble_count, nibble_max, pack_macro_zone, pack_micro_zone, StackedState};
use crate::players::{self, player_profiles as _player_profiles_table};
use crate::souls::{soul_privates as _soul_privates_table, with_portrait, SoulPrivate};

/// World surface band — souls spawn here, on the world hex grid.
/// Mirrors the constant defined in `action_completion.rs` / `actions.rs`.
const WORLD_LAYER: u8 = 64;

/// Inventory surface band — pack contents land here in the new
/// soul's inventory bucket.
const INVENTORY_LAYER: u8 = 1;

/// Default soul cap baked into a new profile in `claim_or_login`.
/// `PlayerProfile.soul_info`'s low nibble is the actual source of
/// truth at runtime — this constant just seeds it. Picked so the
/// roster is a manageable size without turning character management
/// into a content-management chore.
pub const DEFAULT_SOULS_PER_PLAYER: u8 = 5;

/// Create a new character from a starter pack: spawn one soul plus
/// its starting inventory for the calling player. See the module doc
/// for eligibility rules and side-effect details.
#[reducer]
pub fn create_character(
    ctx: &ReducerContext,
    client_time_ms: u64,
    starter_pack_id: StarterPackId,
) -> Result<(), String> {
    let player_id = players::resolve_caller(ctx)?;
    let now_ms = crate::cards::effective_now_ms(ctx, client_time_ms)?;
    // Magnetic block gate — no carve-out. Spawning a new character
    // is card-progression and gated until expired magnetic actions
    // are resolved on the caller's existing souls.
    crate::lifecycle_pending::block_check(ctx, player_id, now_ms, &[])?;

    let pack = starter_pack(starter_pack_id)
        .map_err(|e| format!("create_character: registry: {e}"))?
        .ok_or_else(|| {
            format!("create_character: no starter pack with id {starter_pack_id}")
        })?;

    // Eligibility: the unlock bit for this pack must be set on the
    // caller's profile. Bit position is `id - 1` (id 1 → bit 0; the
    // sentinel `STARTER_PACK_NONE = 0` never participates).
    let Some(profile) = ctx
        .db
        .player_profiles()
        .player_id()
        .find(player_id)
    else {
        return Err(format!(
            "create_character: profile for player {player_id} not found"
        ));
    };
    let bit_pos = (starter_pack_id as u32).saturating_sub(1);
    if bit_pos >= 64 {
        return Err(format!(
            "create_character: pack id {starter_pack_id} exceeds the 64-bit unlock field"
        ));
    }
    let bit = 1u64 << bit_pos;
    if profile.starter_packs & bit == 0 {
        return Err(format!(
            "create_character: pack {} ({}/{}) not unlocked for player {player_id}",
            starter_pack_id, pack.soul, pack.pack_id
        ));
    }

    // Cap: per-profile `soul_info.max`. Checked AFTER unlock so the
    // error message tells the player the most useful thing first
    // ("you haven't unlocked this pack" beats "you're at the soul
    // cap"). Count + max are packed into one byte and maintained by
    // `crate::souls::on_card_write`, so this is a single row read
    // — no `cards.owner_id` scan.
    let max_souls = nibble_max(profile.soul_info);
    let current_count = nibble_count(profile.soul_info);
    if current_count >= max_souls {
        return Err(format!(
            "create_character: player {player_id} already owns {current_count} souls \
             (max {max_souls})"
        ));
    }

    // ---- Spawn the soul card ---------------------------------------
    //
    // World-rooted at origin. `FLAG_OWNED_BY_PLAYER` makes
    // `cards::owning_player` terminate here — the new soul is the
    // first row in its container chain.
    let soul_def = find_packed_by_key(&pack.soul)
        .map_err(|e| format!("create_character: soul {:?}: {e}", pack.soul))?
        .ok_or_else(|| {
            format!(
                "create_character: soul card def {:?} not registered",
                pack.soul
            )
        })?;
    let soul_card_id = cards::next_card_id(ctx);
    // Deterministic 4-bit portrait pick. Mixing the soul's
    // freshly-allocated card id with `now_ms` and `player_id` gives
    // a fresh value per soul without needing an rng (SpacetimeDB
    // reducers stay deterministic; consecutive souls for the same
    // player land on different portraits because `card_id`
    // monotonically increments). See [`cards/flags.json`] →
    // `cards.portrait_id` for the field layout.
    let portrait_seed = (now_ms as u32)
        ^ (now_ms >> 32) as u32
        ^ player_id
        ^ soul_card_id;
    let portrait_id = ((portrait_seed ^ (portrait_seed >> 4)) & 0xF) as u8;
    let soul_flags_state = with_portrait(state_flags().is_owned_by_player, portrait_id);
    cards::create_at(
        ctx,
        soul_card_id,
        now_ms,
        /* surface         */ WORLD_LAYER,
        /* macro_zone      */ pack_macro_zone(0, 0),
        /* micro_zone      */ pack_micro_zone(0, 0, StackedState::Free),
        /* micro_location  */ 0,
        /* owner_id        */ player_id,
        soul_def,
        /* flags_state     */ soul_flags_state,
        /* flags_bk        */ 0,
    );

    // Seed the per-soul private state row. The soul's starter
    // blueprints (from the `"blueprints": [...]` array under its
    // entry in `content/starter_packs/data/*.json`) get packed into
    // the `blueprints_0` bit field — bit position = `blueprint_id -
    // 1` (so id 1 → bit 0), matching the 1-indexed id mapping in
    // `content/blueprints/id.json`. Ids 1..=64 fit in `blueprints_0`;
    // anything beyond that is silently dropped today, but the bit
    // field is sized to grow (`blueprints_1`, …) when the catalog
    // crosses 64. The owning client subscribes to its own soul's row
    // via `WHERE card_id = <soul card_id>`.
    let starter_blueprints = starter_blueprints_for_soul(&pack.soul)
        .map_err(|e| format!("create_character: starter blueprints: {e}"))?;
    let mut blueprints_0: u64 = 0;
    for bp_id in starter_blueprints {
        // `BLUEPRINT_NONE` (0) is a sentinel and can't appear here —
        // `starter_blueprints_for_soul` filters via `find_blueprint`
        // which returns `Some(Blueprint { id: 1.. })` for real keys.
        // Guard anyway so a future content shape change can't
        // underflow `bit_pos`.
        if bp_id == 0 {
            continue;
        }
        let bit_pos = (bp_id as u32) - 1;
        if bit_pos < 64 {
            blueprints_0 |= 1u64 << bit_pos;
        }
    }
    ctx.db.soul_privates().insert(SoulPrivate {
        card_id: soul_card_id,
        blueprints_0,
        active_blueprints: 0,
    });

    // ---- Spawn the pack contents into the soul's inventory --------
    //
    // Each item lands loose at the soul's inventory address; client
    // layout assigns local xy from there. OnCreate recipe matching is
    // now client-driven — the client scans root-only recipes against
    // each newly-created card and submits a `propose_action` if any
    // apply.
    for item in &pack.contents {
        for _ in 0..item.count {
            let card_id = cards::next_card_id(ctx);
            cards::create_at(
                ctx,
                card_id,
                now_ms,
                /* surface         */ INVENTORY_LAYER,
                /* macro_zone      */ soul_card_id,
                /* micro_zone      */ 0,
                /* micro_location  */ 0,
                /* owner_id        */ soul_card_id,
                item.packed_definition,
                /* flags_state     */ 0,
                /* flags_bk        */ 0,
            );
        }
    }

    Ok(())
}
