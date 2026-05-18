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
//! - **Soul cap.** A player may own at most [`MAX_SOULS_PER_PLAYER`]
//!   alive souls at once. Counted by walking
//!   `cards.owner_id == player_id` and filtering on
//!   `FLAG_OWNED_BY_PLAYER` + non-dead.
//!
//! # Side effects (success path)
//!
//! 1. A new soul card lands at world origin: `surface = WORLD_LAYER`,
//!    `macro_zone = (0, 0)`, `micro_zone = OnHex(0, 0)`,
//!    `owner_id = caller's player_id`, `flags = FLAG_OWNED_BY_PLAYER`.
//! 2. For each `StarterPackItem`, `count` copies are spawned in the
//!    soul's inventory: `surface = INVENTORY_LAYER`,
//!    `macro_zone = owner_id = soul.card_id`, `micro_zone = 0`,
//!    `flags = 0`.
//! 3. Every spawn fans out through `cards::create` →
//!    `on_create::trigger`, so OnCreate recipes wired on the soul or
//!    its starting cards fire automatically.
//!
//! The unlock bit is **not** cleared on character creation — a pack
//! stays usable as long as it's unlocked and the player has room
//! under the soul cap. That's what supports "delete a character and
//! roll another from the same pack."

use spacetimedb::{reducer, ReducerContext};

use resonantdust_content::definition_core::find_packed_by_key;
use resonantdust_content::starter_pack_core::{starter_pack, StarterPackId};

use crate::cards::{self, cards as _cards_table};
use crate::packed::{pack_macro_zone, pack_micro_zone, StackedState};
use crate::players::{self, player_profiles as _player_profiles_table};
use crate::souls::with_portrait;

/// World surface band — souls spawn here, on the world hex grid.
/// Mirrors the constant defined in `action_completion.rs` / `actions.rs`.
const WORLD_LAYER: u8 = 64;

/// Inventory surface band — pack contents land here in the new
/// soul's inventory bucket.
const INVENTORY_LAYER: u8 = 1;

/// `dead` flag bit (see `content/cards/flags.json`). A card whose
/// latest row carries this is excluded from the soul count.
const FLAG_DEAD: u32 = 1 << 7;

/// Hard cap on concurrent soul cards per player. Tunable — pick a
/// number we're comfortable defending in UI / subscription cost /
/// inventory addressing terms. 5 leaves plenty of room without
/// turning the character roster into a content-management chore.
pub const MAX_SOULS_PER_PLAYER: usize = 5;

fn now_ms(ctx: &ReducerContext) -> u64 {
    (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64
}

/// Count alive souls owned by `player_id`. Walks the `owner_id`
/// btree index (soul cards carry `owner_id = player_id`), dedupes
/// version rows by `card_id`, and counts only cards whose **latest**
/// row carries `FLAG_OWNED_BY_PLAYER`, is non-dead, and still
/// matches `owner_id == player_id` (defensive — a card that briefly
/// held this owner_id in history but has since moved on shouldn't
/// count).
fn count_souls(ctx: &ReducerContext, player_id: u32) -> usize {
    let mut seen: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut count = 0usize;
    for row in ctx.db.cards().owner_id().filter(player_id) {
        if !seen.insert(row.card_id) {
            continue;
        }
        let Some(latest) = cards::latest(ctx, row.card_id) else {
            continue;
        };
        if latest.flags & FLAG_DEAD != 0 {
            continue;
        }
        if latest.flags & cards::FLAG_OWNED_BY_PLAYER == 0 {
            continue;
        }
        if latest.owner_id != player_id {
            continue;
        }
        count += 1;
    }
    count
}

/// Create a new character from a starter pack: spawn one soul plus
/// its starting inventory for the calling player. See the module doc
/// for eligibility rules and side-effect details.
#[reducer]
pub fn create_character(
    ctx: &ReducerContext,
    starter_pack_id: StarterPackId,
) -> Result<(), String> {
    let player_id = players::resolve_caller(ctx)?;
    // Magnetic block gate — no carve-out. Spawning a new character
    // is card-progression and gated until expired magnetic actions
    // are resolved on the caller's existing souls.
    crate::lifecycle_pending::block_check(ctx, player_id, now_ms(ctx), &[])?;

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

    // Cap: 5 souls per player. Checked AFTER unlock so the error
    // message tells the player the most useful thing first ("you
    // haven't unlocked this pack" beats "you're at the soul cap").
    let current_count = count_souls(ctx, player_id);
    if current_count >= MAX_SOULS_PER_PLAYER {
        return Err(format!(
            "create_character: player {player_id} already owns {current_count} souls (max {MAX_SOULS_PER_PLAYER})"
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
    let now = now_ms(ctx);
    let portrait_seed = (now as u32)
        ^ (now >> 32) as u32
        ^ player_id
        ^ soul_card_id;
    let portrait_id = ((portrait_seed ^ (portrait_seed >> 4)) & 0xF) as u8;
    let soul_flags = with_portrait(cards::FLAG_OWNED_BY_PLAYER, portrait_id);
    cards::create(
        ctx,
        soul_card_id,
        /* surface         */ WORLD_LAYER,
        /* macro_zone      */ pack_macro_zone(0, 0),
        /* micro_zone      */ pack_micro_zone(0, 0, StackedState::OnHex),
        /* micro_location  */ 0,
        /* owner_id        */ player_id,
        soul_def,
        /* flags           */ soul_flags,
    );
    crate::on_create::trigger(ctx, soul_card_id, player_id, now_ms(ctx))?;

    // ---- Spawn the pack contents into the soul's inventory --------
    //
    // Each item lands loose at the soul's inventory address; client
    // layout assigns local xy from there. `on_create::trigger` runs
    // per-card so cards carrying OnCreate recipes (e.g. `fleeting`)
    // get their completions queued.
    for item in &pack.contents {
        for _ in 0..item.count {
            let card_id = cards::next_card_id(ctx);
            cards::create(
                ctx,
                card_id,
                /* surface         */ INVENTORY_LAYER,
                /* macro_zone      */ soul_card_id,
                /* micro_zone      */ 0,
                /* micro_location  */ 0,
                /* owner_id        */ soul_card_id,
                item.packed_definition,
                /* flags           */ 0,
            );
            crate::on_create::trigger(ctx, card_id, player_id, now_ms(ctx))?;
        }
    }

    Ok(())
}
