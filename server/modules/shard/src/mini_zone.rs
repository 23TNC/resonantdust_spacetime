//! Mini-zone deploy / pickup reducers.
//!
//! A mini_zone is a radius-3 hex disk (37 cells) overlaid on the
//! world, anchored to a card whose `card_type == mini_zone`. The
//! Zone row that backs a mini_zone lives at
//! `(surface = MINI_ZONE_LAYER, macro_zone = anchor.card_id)` — the
//! anchor's `card_id` doubles as the mini_zone's identifier inside
//! the surface=63 namespace. Cards placed onto a mini_zone tile
//! follow the same convention (`surface=63, macro_zone=anchor.card_id,
//! micro_zone=offset`).
//!
//! See the design discussion in the conversation for the full
//! rationale; this module wires it up for v1 with:
//!
//! - `deploy_mini_zone` — moves an anchor card out of inventory onto
//!   a world hex and creates an empty mini_zone Zone row.
//! - `pickup_mini_zone` — moves any cards still on the mini_zone
//!   onto the anchor's previous world hex, deletes the Zone row,
//!   moves the anchor back to inventory.
//!
//! Known v1 limitations (intentional — will be revisited):
//!
//! - Deploy doesn't check whether the target hex is already covered
//!   by another mini_zone's footprint. Position-resolver work is in
//!   a follow-up phase; until then players can overlap by accident.
//! - Pickup spills contents to the anchor's previous world hex as a
//!   single pile (every card at `surface=64, macro_zone=anchor.macro_zone,
//!   micro_zone=anchor.micro_zone, micro_location=0`). Visually a
//!   stack; we'll refine to fan-out / inventory-return once we have a
//!   policy.
//! - No permission check beyond "actor owns the anchor card." Cards
//!   that other players left on the mini_zone get spilled to the
//!   world hex (preserving each card's `owner_id`).

use std::sync::OnceLock;

use resonantdust_content::definition_core::{
    card_type_ids, decode_definition,
};
use spacetimedb::{reducer, ReducerContext};

use crate::action_completion;
use crate::cards::{self, cards as _cards_table, Card};
use crate::packed::{
    self, pack_macro_zone, pack_zone_definition, unpack_macro_zone, unpack_micro_zone,
    StackedState, INVENTORY_LAYER, MINI_ZONE_LAYER, WORLD_LAYER,
};
use crate::players;
use crate::zones::{self, zones as _zones_table};

/// Mini_zone footprint: hex disk of radius 3 (37 cells) centered at
/// `(3, 3)` within an 8×8 tile-byte storage grid.
const MINI_ZONE_RADIUS: i32 = 3;
const MINI_ZONE_CENTER: i32 = 3;

/// Macro_zone offsets to scan when looking for mini_zone anchors that
/// might cover a target world hex. The target's own macro_zone plus
/// the 6 axial-hex neighbors — a radius-3 footprint can reach into
/// any of these from an anchor near the chunk boundary.
const ADJACENT_MACRO_ZONES: [(i16, i16); 7] = [
    (0, 0),
    (1, 0),
    (-1, 0),
    (0, 1),
    (0, -1),
    (1, -1),
    (-1, 1),
];

// `cards/flags.json` bit positions. Local — same per-file pattern the
// rest of the codebase uses.
const FLAG_DEAD: u32 = 1 << 7;

/// Look up the `card_type` id for `"mini_zone"` once and cache.
/// Returns `None` if the type isn't registered in the content
/// catalog (which would be a content-build error, not a runtime
/// path the reducers ever hit).
static MINI_ZONE_TYPE_ID: OnceLock<Option<u8>> = OnceLock::new();
fn mini_zone_type_id() -> Option<u8> {
    *MINI_ZONE_TYPE_ID.get_or_init(|| {
        card_type_ids()
            .ok()
            .and_then(|m| m.get("mini_zone").copied())
    })
}

/// True iff this card's definition is of type `mini_zone`.
pub fn is_mini_zone_card(packed_definition: u16) -> bool {
    let Some(type_id) = mini_zone_type_id() else {
        return false;
    };
    decode_definition(packed_definition)
        .ok()
        .flatten()
        .is_some_and(|def| def.card_type == type_id)
}

/// Cube-distance between two axial hex coords.
fn hex_distance(a: (i32, i32), b: (i32, i32)) -> i32 {
    let dq = a.0 - b.0;
    let dr = a.1 - b.1;
    (dq.abs() + dr.abs() + (dq + dr).abs()) / 2
}

/// Convert `(macro_zone, micro_zone)` to a global hex coord
/// `(global_q, global_r)`. Each macro_zone is an 8×8 chunk; the
/// global axis is `macro * 8 + local`.
fn global_hex(macro_zone: u32, micro_zone: u8) -> (i32, i32) {
    let (mq, mr) = unpack_macro_zone(macro_zone);
    let (lq, lr, _) = unpack_micro_zone(micro_zone);
    (mq as i32 * 8 + lq as i32, mr as i32 * 8 + lr as i32)
}

/// Find a mini_zone anchor card whose footprint covers the world
/// hex at `(world_macro_zone, world_micro_zone)`. Scans the target
/// macro_zone and its 6 axial neighbors (a radius-3 footprint can
/// reach into the target chunk from anchors placed at most one
/// chunk away).
///
/// **v1 limitation:** at most one mini_zone may cover any world hex
/// (overlap isn't validated at deploy time — see
/// `deploy_mini_zone`'s TODO). If two anchors' footprints overlap
/// at this hex, the first one encountered by the macro_zone scan
/// wins. Deterministic given a fixed iteration order; not
/// meaningful otherwise.
pub fn anchor_covering_hex(
    ctx: &ReducerContext,
    world_macro_zone: u32,
    world_micro_zone: u8,
) -> Option<Card> {
    let target_global = global_hex(world_macro_zone, world_micro_zone);
    let (target_macro_q, target_macro_r) = unpack_macro_zone(world_macro_zone);

    for (dmq, dmr) in ADJACENT_MACRO_ZONES {
        let scan_macro_q = target_macro_q.saturating_add(dmq);
        let scan_macro_r = target_macro_r.saturating_add(dmr);
        let scan_macro_zone = pack_macro_zone(scan_macro_q, scan_macro_r);

        // Walk every card row at this macro_zone. Multiple history
        // rows per card_id appear here; dedupe via card_id BTreeSet
        // + `cards::latest` so each candidate is evaluated once at
        // its current state.
        let mut seen: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for c in ctx.db.cards().macro_zone().filter(scan_macro_zone) {
            if !seen.insert(c.card_id) {
                continue;
            }
            if !is_mini_zone_card(c.packed_definition) {
                continue;
            }
            let Some(latest) = cards::latest(ctx, c.card_id) else {
                continue;
            };
            if latest.surface != WORLD_LAYER {
                continue;
            }
            if latest.macro_zone != scan_macro_zone {
                continue;
            }
            // Anchor on a different macro_zone now (history-row drift) —
            // its current home isn't in our scan window; skip.
            let anchor_global = global_hex(latest.macro_zone, latest.micro_zone);
            if hex_distance(anchor_global, target_global) <= MINI_ZONE_RADIUS {
                return Some(latest);
            }
        }
    }
    None
}

/// Given an anchor card known to cover the target world hex (see
/// [`anchor_covering_hex`]), look up the mini_zone Zone, compute
/// the `(q, r)` within the mini_zone's 7×7 grid, and return the
/// tile-def packed value + `HexLocation` for action_completion.
///
/// Returns `None` if the mini_zone Zone doesn't exist (would be a
/// bug — anchor without a backing zone) or the corresponding tile
/// byte is `0` (empty cell of the mini_zone; mini_zone occludes
/// the world tile underneath, so no synthetic hex is produced — a
/// recipe targeting "this hex" finds nothing).
pub fn tile_at_anchor(
    ctx: &ReducerContext,
    anchor: &Card,
    world_macro_zone: u32,
    world_micro_zone: u8,
) -> Option<(u16, action_completion::HexLocation)> {
    let zone = zones::latest_for(ctx, MINI_ZONE_LAYER, anchor.card_id)?;
    let target_global = global_hex(world_macro_zone, world_micro_zone);
    let anchor_global = global_hex(anchor.macro_zone, anchor.micro_zone);
    let dq = target_global.0 - anchor_global.0;
    let dr = target_global.1 - anchor_global.1;
    let mz_q = (MINI_ZONE_CENTER + dq) as u8;
    let mz_r = (MINI_ZONE_CENTER + dr) as u8;
    let tile_def_id = zone.tile_at(mz_r, mz_q).unwrap_or(0);
    if tile_def_id == 0 {
        return None;
    }
    // Same packed_def synthesis world zones use: shift the zone's
    // `[card_type:u4 | 0:u4]` byte left 8 — the type bits land at
    // u16 positions 12..15, leaving room for the u12 def_id below.
    let packed_def = ((zone.packed_definition as u16) << 8) | tile_def_id;
    let location = action_completion::HexLocation {
        zone_id: zone.zone_id,
        macro_zone: zone.macro_zone,
        col: mz_q,
        row: mz_r,
        owner_id: zone.owner_id,
    };
    Some((packed_def, location))
}

/// Deploy a mini_zone-anchor card from the caller's inventory onto
/// a world hex.
///
/// **Steps:**
/// 1. Resolve caller → player_id + soul card.
/// 2. Validate `anchor_card_id`: exists, owned by caller, is a
///    `mini_zone`-type card, currently in caller's inventory
///    (`surface == INVENTORY_LAYER`).
/// 3. Validate the target hex: `target_micro_zone` decodes to a
///    valid `OnHex` byte, no card already occupies the world hex
///    at `(target_macro_zone, target_micro_zone)`.
/// 4. Allocate a fresh `zone_id`.
/// 5. Rewrite the anchor card's row to land at world
///    `(surface=WORLD_LAYER, macro_zone=target_macro_zone,
///      micro_zone=target_micro_zone, state=OnHex, micro_location=0)`.
/// 6. Create the mini_zone `Zone` row at
///    `(surface=MINI_ZONE_LAYER, macro_zone=anchor.card_id)` with all
///    37 effective hex cells set to `def_id == 0` (empty). The Zone
///    storage is 8 × u64 = 64 tile bytes; cells outside the radius-3
///    hex disk (the 12 corner positions in a 7×7 grid plus the 15
///    padding bytes from the 8×8 storage layout) are not addressable
///    by gameplay and stay 0.
#[reducer]
pub fn deploy_mini_zone(
    ctx: &ReducerContext,
    anchor_card_id: u32,
    target_macro_zone: u32,
    target_micro_zone: u8,
) -> Result<(), String> {
    // ---- caller resolution ----------------------------------------
    let caller_player_id = players::resolve_caller(ctx)?;
    // Magnetic block gate — no carve-out. Deploying a mini-zone is
    // an economic progression action and gated on expired-magnetic
    // resolution.
    crate::lifecycle_pending::block_check(
        ctx,
        caller_player_id,
        cards::now_ms(ctx),
        &[],
    )?;

    // ---- anchor validation ----------------------------------------
    let anchor = cards::latest(ctx, anchor_card_id).ok_or_else(|| {
        format!("deploy_mini_zone: anchor card {anchor_card_id} not found")
    })?;
    if anchor.flags & FLAG_DEAD != 0 {
        return Err(format!(
            "deploy_mini_zone: anchor card {anchor_card_id} is dead"
        ));
    }
    // Ownership: walk up via `owning_player` to verify the anchor
    // ultimately belongs to the caller. Under the post-flag-20
    // card-owner model, anchor.owner_id is a card_id (the soul that
    // contains it), not a player_id directly.
    let anchor_player = cards::owning_player(ctx, anchor_card_id)
        .unwrap_or(cards::WORLD_PLAYER_ID);
    if anchor_player != caller_player_id {
        return Err(format!(
            "deploy_mini_zone: anchor card {anchor_card_id} is owned by player {anchor_player} (not {caller_player_id})"
        ));
    }
    if !is_mini_zone_card(anchor.packed_definition) {
        return Err(format!(
            "deploy_mini_zone: card {anchor_card_id} is not a `mini_zone`-type card"
        ));
    }
    if anchor.surface != INVENTORY_LAYER {
        return Err(format!(
            "deploy_mini_zone: anchor card {anchor_card_id} must be in inventory \
             (surface={INVENTORY_LAYER}); current surface={}",
            anchor.surface
        ));
    }

    // ---- target validation ----------------------------------------
    let (_t_q, _t_r, t_state) = unpack_micro_zone(target_micro_zone);
    if t_state != StackedState::OnHex {
        return Err(format!(
            "deploy_mini_zone: target micro_zone must carry state=OnHex; got {t_state:?}"
        ));
    }
    // Confirm a world Zone exists at the target macro_zone — placing
    // on unmapped area is rejected.
    if zones::latest_for(ctx, WORLD_LAYER, target_macro_zone).is_none() {
        return Err(format!(
            "deploy_mini_zone: no world zone exists at macro_zone={target_macro_zone}; \
             cannot deploy in unmapped area"
        ));
    }
    // Reject if any card already occupies the target hex on the
    // world layer. `state == OnHex` cards at the same micro_zone =
    // this hex's resident.
    let occupied = ctx
        .db
        .cards()
        .macro_zone()
        .filter(target_macro_zone)
        .any(|c| {
            if c.surface != WORLD_LAYER {
                return false;
            }
            if c.flags & FLAG_DEAD != 0 {
                return false;
            }
            // Compare via latest() so we don't trip on stale history rows.
            let Some(latest) = cards::latest(ctx, c.card_id) else {
                return false;
            };
            latest.surface == WORLD_LAYER
                && latest.macro_zone == target_macro_zone
                && latest.micro_zone == target_micro_zone
                && latest.flags & FLAG_DEAD == 0
        });
    if occupied {
        return Err(format!(
            "deploy_mini_zone: target hex (macro_zone={target_macro_zone}, \
             micro_zone={target_micro_zone}) already has an occupant"
        ));
    }

    // ---- write the anchor onto the world hex ----------------------
    //
    // The anchor leaves inventory and lands at the world position.
    // State becomes OnHex, micro_location=0 (no parent card).
    // `owner_id` is intentionally untouched — physical location
    // (surface / macro_zone / micro_zone) and ownership are
    // orthogonal: a card can sit on a world hex and still be
    // soul-owned, which is exactly what we want here so the
    // deploying player retains pickup permission (`owning_player`
    // walks `owner_id` upward and finds them). World-owned things
    // (trees, rocks) are world-owned by construction in world_gen,
    // not by being on the world surface. Position-preserve /
    // position-locked are NOT set — the user can still pick the
    // wagon back up via `pickup_mini_zone`.
    cards::update_with(ctx, anchor_card_id, |c| {
        c.surface = WORLD_LAYER;
        c.macro_zone = target_macro_zone;
        c.micro_zone = target_micro_zone;
        c.micro_location = 0;
    });

    // ---- create the mini_zone Zone row ----------------------------
    //
    // `surface = MINI_ZONE_LAYER`, `macro_zone = anchor.card_id` —
    // the anchor's `card_id` doubles as the mini_zone's identifier
    // in the surface=63 namespace. All 8 tile rows initialized to
    // zero (empty); future phases will populate via a separate
    // mechanism (template on def / seed reducer / etc.).
    //
    // `packed_definition` on the Zone: matches the world-zone shape
    // `pack_zone_definition(card_type=tile)` so tile-byte lookups
    // (which OR the zone's packed_definition with the tile byte to
    // build a full packed_def) produce a tile-kind card_def the
    // recipe matcher can resolve. `card_type id 7 == "tile"` per
    // content/cards/types.json; keep this in sync if those ids ever
    // change.
    let tile_zone_packed_def = pack_zone_definition(/* tile */ 7);
    let zone_id = zones::next_zone_id(ctx);
    zones::create(
        ctx,
        zone_id,
        MINI_ZONE_LAYER,
        anchor_card_id,
        tile_zone_packed_def,
        // `Zone.owner_id` is a card_id under the new model — the
        // anchor itself is the container card for the mini_zone's
        // contents. (No `FLAG_OWNED_BY_PLAYER` analog on Zone rows;
        // they're always card_id-keyed.)
        /* owner_id */ anchor_card_id,
        /* tiles */ [0; crate::packed::ZONE_TILE_U64_COUNT],
    );

    Ok(())
}

/// Pick up a deployed mini_zone — move every card sitting on its
/// tiles to the anchor's previous world hex, delete the Zone row,
/// move the anchor back to the caller's inventory.
///
/// **Steps:**
/// 1. Resolve caller → player_id.
/// 2. Validate anchor exists, owned by caller, mini_zone type, on
///    world layer.
/// 3. For every alive card on the mini_zone (`surface=63,
///    macro_zone=anchor.card_id`): rewrite to land at the anchor's
///    previous world hex, `state=OnHex, micro_location=0`. (v1
///    spill — multiple cards pile up on the same world hex.
///    Refinement deferred.)
/// 4. Delete the Zone row at `(surface=63, macro_zone=anchor.card_id)`.
/// 5. Rewrite the anchor card to land in inventory.
#[reducer]
pub fn pickup_mini_zone(ctx: &ReducerContext, anchor_card_id: u32) -> Result<(), String> {
    let caller_player_id = players::resolve_caller(ctx)?;

    let anchor = cards::latest(ctx, anchor_card_id).ok_or_else(|| {
        format!("pickup_mini_zone: anchor card {anchor_card_id} not found")
    })?;
    if anchor.flags & FLAG_DEAD != 0 {
        return Err(format!(
            "pickup_mini_zone: anchor card {anchor_card_id} is dead"
        ));
    }
    // Ownership: walk up via `owning_player` to verify the anchor
    // ultimately belongs to the caller. The wagon stays soul-owned
    // when deployed (deploy_mini_zone leaves `owner_id` alone), so
    // the walk lands on the deploying soul → its player.
    let anchor_player = cards::owning_player(ctx, anchor_card_id)
        .unwrap_or(cards::WORLD_PLAYER_ID);
    if anchor_player != caller_player_id {
        return Err(format!(
            "pickup_mini_zone: anchor card {anchor_card_id} is owned by player {anchor_player} (not {caller_player_id})"
        ));
    }
    // The destination inventory is the same soul that already owns
    // the anchor (the deploying soul) — we read it off the anchor
    // rather than `player.soul_card_id` so the pickup lands in the
    // *right* soul's inventory even when the player has switched
    // active characters since deploying.
    let target_soul_card_id = cards::owning_soul(ctx, anchor_card_id).ok_or_else(|| {
        format!(
            "pickup_mini_zone: anchor card {anchor_card_id} has no soul in its owner chain"
        )
    })?;
    if !is_mini_zone_card(anchor.packed_definition) {
        return Err(format!(
            "pickup_mini_zone: card {anchor_card_id} is not a `mini_zone`-type card"
        ));
    }
    if anchor.surface != WORLD_LAYER {
        return Err(format!(
            "pickup_mini_zone: anchor card {anchor_card_id} must be on world layer \
             (surface={WORLD_LAYER}); current surface={}",
            anchor.surface
        ));
    }

    // Snapshot the anchor's world position so we can spill mini_zone
    // contents onto it before the anchor moves to inventory.
    let world_macro_zone = anchor.macro_zone;
    let world_micro_zone = anchor.micro_zone;

    // ---- spill mini_zone contents to the anchor's world hex -------
    //
    // V1: every alive card on the mini_zone lands at the anchor's
    // previous world hex. They pile up at the same `micro_zone` —
    // visually messy, but correctness-clean (no orphans pointing at
    // a deleted Zone). Each card's `owner_id` is preserved, so the
    // spill works for any cards left by other players too.
    //
    // Iterate via the `macro_zone` btree index. The mini_zone Zone
    // and the cards on it share `macro_zone = anchor.card_id`, so
    // this filter also includes the Zone row's `macro_zone` (we
    // discriminate by surface in the loop).
    let card_ids: Vec<u32> = ctx
        .db
        .cards()
        .macro_zone()
        .filter(anchor_card_id)
        .filter_map(|c| {
            if c.surface != MINI_ZONE_LAYER {
                return None;
            }
            // Skip the anchor itself (it's on WORLD_LAYER, but
            // defensive).
            if c.card_id == anchor_card_id {
                return None;
            }
            Some(c.card_id)
        })
        .collect();
    // Dedup via `cards::latest` lookup — `macro_zone` filter returns
    // every history row, so a card with N rows shows N times. We
    // only want to write each card once.
    let mut seen: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    for id in card_ids {
        if !seen.insert(id) {
            continue;
        }
        let Some(latest) = cards::latest(ctx, id) else {
            continue;
        };
        if latest.surface != MINI_ZONE_LAYER || latest.macro_zone != anchor_card_id {
            // Moved out since we started iterating — leave alone.
            continue;
        }
        if latest.flags & FLAG_DEAD != 0 {
            continue;
        }
        cards::update_with(ctx, id, |c| {
            c.surface = WORLD_LAYER;
            c.macro_zone = world_macro_zone;
            c.micro_zone = world_micro_zone;
            c.micro_location = 0;
        });
    }

    // ---- delete the mini_zone Zone row ----------------------------
    //
    // The Zone lives at `(surface=MINI_ZONE_LAYER, macro_zone=anchor.card_id)`.
    // Multiple history rows may exist; delete them all so the
    // identifier is fully freed.
    let zone_pks: Vec<u64> = ctx
        .db
        .zones()
        .macro_zone()
        .filter(anchor_card_id)
        .filter(|z| z.surface == MINI_ZONE_LAYER)
        .map(|z| z.valid_at)
        .collect();
    for v in zone_pks {
        ctx.db.zones().valid_at().delete(v);
    }

    // ---- return the anchor to inventory ---------------------------
    cards::update_with(ctx, anchor_card_id, |c| {
        c.surface = INVENTORY_LAYER;
        c.macro_zone = target_soul_card_id;
        c.micro_zone = 0;
        c.micro_location = 0;
        // Back in the soul's inventory bucket — `owner_id` carries
        // the soul's card_id (matches the inventory address pun).
        c.owner_id = target_soul_card_id;
    });

    Ok(())
}
