//! `place_card` — the unified card-placement reducer.
//!
//! Replaces the soul-specific `equip_card` / `unequip_card` pair with
//! a single generic primitive: "put this card at this position." The
//! position can be a stack onto any parent card (in any branch
//! direction), or a loose placement at an explicit address. The soul
//! is no longer special — it's just whichever parent the client
//! happened to pass.
//!
//! See `docs/PLACE_CARD_GENERALIZATION.md`.

use spacetimedb::{reducer, ReducerContext, SpacetimeType};

use crate::cards::{self, cards as _cards_table, Micro};
use crate::flags::state_flags;
use crate::packed::PLAYER_INVENTORY_LAYER;
use crate::packed::{
    INVENTORY_LAYER, LOOSE_RECT, MINI_ZONE_LAYER, POCKET_DIMENSION_LAYER, SNAP_HEX,
    STACK_DIR_DOWN, STACK_DIR_HEX, STACK_DIR_UP, WORLD_LAYER,
};
use crate::players;

/// Unpack a wire `xy` u32 (`[x:i16 | y:i16]`) — the loose within-cell offset
/// the client packs into `Placement.xy`. Clamped to the i12 range the loose
/// `micro_location` layout stores (±2047).
fn unpack_xy(xy: u32) -> (i16, i16) {
    let x = (xy >> 16) as u16 as i16;
    let y = xy as u16 as i16;
    (x.clamp(-2048, 2047), y.clamp(-2048, 2047))
}

/// Where the placement puts the source card.
///
/// `kind = 0` → **Stack**: stack source as a child of `parent_id` in
/// the given `direction` (`STACK_DIR_UP / DOWN / HEX`). The other
/// fields are ignored.
///
/// `kind = 1` → **Loose**: place source loose at the explicit address
/// `(surface, macro_zone, q, r, xy)`. `parent_id` / `direction` are
/// ignored. The surface band picks which positional fields matter:
/// inventory uses `xy` (packed `(x, y)`) and `q = r = 0`; world uses
/// `(q, r)` and `xy = 0`.
///
/// Flat-struct encoding (not a Rust enum) keeps the wire format
/// stable across SpacetimeDB schema migrations and avoids the
/// enum-codegen variance between SDK versions. Internally
/// dispatched via the `kind` field.
#[derive(SpacetimeType, Debug, Clone, Copy)]
pub struct Placement {
    pub kind: u8,
    pub parent_id: u32,
    pub direction: u8,
    pub surface: u8,
    pub macro_zone: u64,
    pub q: u8,
    pub r: u8,
    pub xy: u32,
}

const PLACEMENT_STACK: u8 = 0;
const PLACEMENT_LOOSE: u8 = 1;

/// Place a card at a position the caller specifies. Generic
/// successor to `equip_card` / `unequip_card`. The reducer validates
/// the source can be moved and the destination accepts the move,
/// then writes the source's new position. Descendants ride along —
/// their `surface` / `macro_zone` are re-stamped so the sub-chain
/// travels with the root; `owner_id` is independent of position and
/// stays untouched. Chain shape (parent-pointers) is
/// unchanged.
///
/// **Source eligibility** (always required):
/// - Source exists and isn't `dead`.
/// - Caller's `player_id` owns the source (`cards::owning_player`
///   walk).
/// - Source isn't held by an in-flight action — `slot_hold_count`,
///   `slot_share_count`, `position_hold_count` all zero on the
///   source AND every descendant in its sub-chain. Moving a
///   recipe-bound card would break the recipe's path.
///
/// **`Stack` destination**:
/// - `parent_id` exists and isn't `dead`.
/// - `direction` is `STACK_DIR_UP`, `DOWN`, or `HEX`.
/// - Parent's `drop_hold_count == 0` (target accepts stacking).
/// - The chain that source's new parent belongs to is caller-owned
///   (`owning_player` of the chain root is the caller, or root is
///   WORLD-owned and not drop-locked).
/// - No cycle: source is not an ancestor of parent.
///
/// **`Loose` destination**:
/// - `surface` is one of `INVENTORY_LAYER (1)`,
///   `POCKET_DIMENSION_LAYER (32)`, `MINI_ZONE_LAYER (63)`, or
///   `WORLD_LAYER (64)+`.
/// - Inventory: `macro_zone` is a soul `card_id` owned by the
///   caller.
/// - World: any caller-owned source may be placed at any world hex
///   (no per-hex permission today; ownership is implicit in the
///   source's chain root staying within the caller's reach).
///
/// **Idempotency.** If the requested placement matches the source's
/// current state (same surface / macro_zone / micro placement), the
/// write still fires but is a no-op semantically
/// — `cards::write_at`'s dirty-flag diff lands a `data_dirty: false,
/// position_dirty: false` row that subscribers can ignore. Keeps the
/// drag UX smooth on retries / spurious resends.
#[reducer]
pub fn place_card(
    ctx: &ReducerContext,
    client_time_ms: u64,
    card_id: u32,
    placement: Placement,
) -> Result<(), String> {
    let caller_player_id = players::resolve_caller(ctx)?;
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;

    let source = cards::prior_at(ctx, card_id, now_ms)
        .ok_or_else(|| format!("place_card: card {card_id} not found"))?;

    let s = state_flags();
    if source.flags_state & s.dead != 0 {
        return Err(format!("place_card: card {card_id} is dead"));
    }

    let source_owner = cards::owning_player(ctx, card_id).unwrap_or(cards::WORLD_PLAYER_ID);
    if source_owner != caller_player_id {
        return Err(format!(
            "place_card: card {card_id} is owned by player {source_owner} (not {caller_player_id})"
        ));
    }

    if cards::slot_hold_count(source.flags_bk) > 0 {
        return Err(format!(
            "place_card: card {card_id} is exclusively held by an in-flight action"
        ));
    }
    if cards::slot_share_count(source.flags_bk) > 0 {
        return Err(format!(
            "place_card: card {card_id} is shared-held by an in-flight action (borrow/share)"
        ));
    }
    if cards::position_hold_count(source.flags_bk) > 0 {
        return Err(format!(
            "place_card: card {card_id} is position-held by an in-flight action"
        ));
    }

    // Collect the source's stack members up-front so the same set is
    // validated and moved. In the flat-root model "members" are every card
    // whose `micro_location` points at the source as their root — a single
    // `micro_location` btree lookup. A source that is itself a member (or a
    // bare loose card) has no members and this returns empty.
    let descendants = collect_members(ctx, card_id, now_ms);
    for d in &descendants {
        if cards::slot_hold_count(d.flags_bk) > 0 {
            return Err(format!(
                "place_card: descendant card {} is exclusively held by an in-flight action",
                d.card_id
            ));
        }
        if cards::slot_share_count(d.flags_bk) > 0 {
            return Err(format!(
                "place_card: descendant card {} is shared-held by an in-flight action",
                d.card_id
            ));
        }
        if cards::position_hold_count(d.flags_bk) > 0 {
            return Err(format!(
                "place_card: descendant card {} is position-held by an in-flight action",
                d.card_id
            ));
        }
    }

    // Dispatch on placement kind. Returns the destination
    // (surface, macro_zone, Micro placement). `owner_id`
    // is intentionally not part of the destination tuple —
    // ownership is an independent property of a card (controls who
    // may move it), unchanged by placement. Cards that need a
    // different owner go through an explicit ownership-transfer
    // reducer (TODO; today no such reducer exists, so cards keep
    // whatever owner they were created with).
    let (new_surface, new_macro_zone, new_micro) = match placement.kind {
        PLACEMENT_STACK => resolve_stack_target(
            ctx,
            card_id,
            placement.parent_id,
            placement.direction,
            caller_player_id,
            now_ms,
        )?,
        PLACEMENT_LOOSE => resolve_loose_target(
            ctx,
            caller_player_id,
            placement.surface,
            placement.macro_zone,
            placement.q,
            placement.r,
            placement.xy,
        )?,
        other => {
            return Err(format!(
                "place_card: unknown placement kind {other} (expected 0=Stack or 1=Loose)"
            ));
        }
    };

    let full_macro = crate::packed::with_surface(new_macro_zone, new_surface);

    // Source write. `owner_id` is intentionally untouched. `surface` folds
    // into `macro_zone` (bits 24-31). `new_micro` sets `micro_location` + the
    // stacking flag bits together.
    cards::update_with_at(ctx, card_id, now_ms, |c| {
        c.macro_zone = full_macro;
        new_micro.apply(c);
    });

    // Move the source's members along.
    // - **Loose move:** source stays the root; members keep pointing at it and
    //   just travel (re-stamp `macro_zone`).
    // - **Stack move:** source becomes a member of `parent_root`, so its former
    //   members re-root onto `parent_root` too (flat chains have no nesting),
    //   keeping their branch+index. Index collisions are gap-tolerant (rare;
    //   only on combining two non-empty stacks) — the design accepts this over
    //   renumbering. `owner_id` stays untouched throughout.
    match new_micro {
        Micro::Stacked { root: new_root, .. } => {
            for m in &descendants {
                let rerooted = match Micro::of(m) {
                    Micro::Stacked { branch, index, .. } => Micro::Stacked {
                        root: new_root,
                        branch,
                        index,
                    },
                    loose => loose,
                };
                cards::update_with_at(ctx, m.card_id, now_ms, |c| {
                    c.macro_zone = full_macro;
                    rerooted.apply(c);
                });
            }
        }
        Micro::Loose { .. } => {
            for m in &descendants {
                cards::update_with_at(ctx, m.card_id, now_ms, |c| {
                    c.macro_zone = full_macro;
                });
            }
        }
    }

    Ok(())
}

/// Resolve a `Stack` placement to the concrete `(surface, macro_zone, Micro)`
/// write tuple — the source becomes a member of the parent's chain root.
///
/// Walks the parent's chain in `direction` to find the current top —
/// the new source becomes the next slot above it. Replicates
/// `equip_card`'s attach shape (`OnRoot` for first child of a Free
/// root; `Slot` for subsequent children). `owner_id` is not part of
/// the return because placement doesn't change ownership.
fn resolve_stack_target(
    ctx: &ReducerContext,
    source_id: u32,
    parent_id: u32,
    direction: u8,
    caller_player_id: u32,
    now_ms: u64,
) -> Result<(u8, u64, Micro), String> {
    if parent_id == 0 {
        return Err("place_card: Stack placement with parent_id == 0".to_string());
    }
    if parent_id == source_id {
        return Err(format!(
            "place_card: card {source_id} can't stack onto itself"
        ));
    }
    if !matches!(direction, STACK_DIR_UP | STACK_DIR_DOWN | STACK_DIR_HEX) {
        return Err(format!(
            "place_card: invalid direction {direction} (expected UP=1, DOWN=2, HEX=0)"
        ));
    }

    let parent = cards::prior_at(ctx, parent_id, now_ms)
        .ok_or_else(|| format!("place_card: parent card {parent_id} not found"))?;
    let s = state_flags();
    if parent.flags_state & s.dead != 0 {
        return Err(format!("place_card: parent card {parent_id} is dead"));
    }
    if cards::drop_hold_count(parent.flags_bk) > 0 {
        return Err(format!(
            "place_card: parent card {parent_id} blocks stacking (drop_hold_count > 0)"
        ));
    }

    // The chain's root: parent itself if it's loose, else the root it points
    // at. Flat chains — one hop, no walk.
    let parent_root = chain_root_of(&parent);

    // Cycle check: source must not be the root of parent's chain (you can't
    // stack a chain onto one of its own members).
    if parent_root == source_id {
        return Err(format!(
            "place_card: stacking would form a cycle (source {source_id} is the root of parent {parent_id}'s chain)"
        ));
    }

    // Ownership: the chain root must be caller-owned (or WORLD-owned, e.g. a
    // tile-card the player is targeting). `owning_player` walks owner_id.
    let chain_player =
        cards::owning_player(ctx, parent_root).unwrap_or(cards::WORLD_PLAYER_ID);
    if chain_player != cards::WORLD_PLAYER_ID && chain_player != caller_player_id {
        return Err(format!(
            "place_card: parent {parent_id}'s chain is owned by player {chain_player} (not {caller_player_id})"
        ));
    }

    // Append at the end of the branch (next free index). Gap-tolerant; never
    // renumbers existing members.
    let index = next_branch_index(ctx, parent_root, parent.macro_zone, direction, now_ms);

    Ok((
        crate::packed::surface_of(parent.macro_zone),
        parent.macro_zone,
        Micro::Stacked {
            root: parent_root,
            branch: direction,
            index,
        },
    ))
}

/// The root card_id of `card`'s chain — `micro_location` if the card is a stack
/// member, else the card itself (it's a loose/snapped root).
fn chain_root_of(card: &cards::Card) -> u32 {
    if cards::micro_is_card(card) {
        card.micro_location
    } else {
        card.card_id
    }
}

/// Every card that is a stack member of `root_id` (flat: `micro_location ==
/// root_id` AND `micro_is_card`). Deduped by card_id at the latest row. Single
/// `micro_location` btree lookup — the core flat-chain enumeration.
fn collect_members(ctx: &ReducerContext, root_id: u32, now_ms: u64) -> Vec<cards::Card> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut out: Vec<cards::Card> = Vec::new();
    for row in ctx.db.cards().micro_location().filter(root_id) {
        if !seen.insert(row.card_id) {
            continue;
        }
        let Some(latest) = cards::prior_at(ctx, row.card_id, now_ms) else {
            continue;
        };
        if latest.micro_location != root_id || !cards::micro_is_card(&latest) {
            continue;
        }
        out.push(latest);
    }
    out
}

/// Next free `stack_index` in `(root_id, direction)` — `max occupied + 1`,
/// saturating at 15; `0` when the branch is empty. Append-to-end; gap-tolerant.
fn next_branch_index(
    ctx: &ReducerContext,
    root_id: u32,
    macro_zone: u64,
    direction: u8,
    now_ms: u64,
) -> u8 {
    let mut max: Option<u8> = None;
    for m in collect_members(ctx, root_id, now_ms) {
        if m.macro_zone != macro_zone {
            continue;
        }
        if cards::stack_branch(&m) == direction {
            let idx = cards::stack_index(&m);
            max = Some(max.map_or(idx, |cur| cur.max(idx)));
        }
    }
    match max {
        Some(m) => ((m as u16 + 1).min(15)) as u8,
        None => 0,
    }
}

/// Resolve a `Loose` placement to the concrete write tuple.
/// `owner_id` is not part of the return — placement doesn't change
/// who owns the card.
fn resolve_loose_target(
    ctx: &ReducerContext,
    caller_player_id: u32,
    surface: u8,
    macro_zone: u64,
    q: u8,
    r: u8,
    xy: u32,
) -> Result<(u8, u64, Micro), String> {
    match surface {
        INVENTORY_LAYER => {
            // The owner band holds the soul's card_id; the soul must belong to caller.
            let soul_player = cards::owning_player(ctx, crate::packed::owner_of(macro_zone)).unwrap_or(cards::WORLD_PLAYER_ID);
            if soul_player != caller_player_id {
                return Err(format!(
                    "place_card: inventory target soul {macro_zone} is owned by player {soul_player} (not {caller_player_id})"
                ));
            }
            // Inventory is a rect grid; the item sits loose at pixel offset
            // (x, y) within cell (0, 0).
            let (x, y) = unpack_xy(xy);
            Ok((INVENTORY_LAYER, macro_zone, Micro::Loose { local_q: 0, local_r: 0, x, y, kind: LOOSE_RECT }))
        }
        PLAYER_INVENTORY_LAYER => {
            // The owner band holds the player_id — must match the caller to
            // prevent dropping into another player's bag.
            if crate::packed::owner_of(macro_zone) != caller_player_id {
                return Err(format!(
                    "place_card: player-inventory target {macro_zone} is not the caller's player_id ({caller_player_id})"
                ));
            }
            let (x, y) = unpack_xy(xy);
            Ok((PLAYER_INVENTORY_LAYER, macro_zone, Micro::Loose { local_q: 0, local_r: 0, x, y, kind: LOOSE_RECT }))
        }
        s if s >= WORLD_LAYER => {
            if q >= 8 || r >= 8 {
                return Err(format!(
                    "place_card: world target ({q}, {r}) out of range (0..=7 each)"
                ));
            }
            // Player-dropped world card snaps to the hex centre. `SNAP_HEX`
            // tells the renderer to ignore the within-cell `(x, y)` offset
            // (so the card always renders centred even if `xy` carries
            // non-zero data) — matches the client's
            // `looseKindForSurface(WORLD_LAYER) → SNAP_HEX` hardcode. We
            // still pass through `(x, y)` from the request so the row's
            // payload stays consistent with whatever the client sent, but
            // it's effectively ignored at render time. Structures that
            // should snap-and-stack onto the tile still go through recipe
            // placement, not this drag path.
            let (x, y) = unpack_xy(xy);
            Ok((surface, macro_zone, Micro::Loose { local_q: q, local_r: r, x, y, kind: SNAP_HEX }))
        }
        MINI_ZONE_LAYER | POCKET_DIMENSION_LAYER => {
            // Mini-zone / pocket-dimension placement: `macro_zone` is the anchor
            // card's `card_id`. Require a caller-owned anchor (same gate as
            // inventory). Mini-zone uses hex coords (q, r); pocket-dimension is
            // a rect interior using the xy offset.
            let anchor_player =
                cards::owning_player(ctx, crate::packed::owner_of(macro_zone)).unwrap_or(cards::WORLD_PLAYER_ID);
            if anchor_player != caller_player_id {
                return Err(format!(
                    "place_card: container anchor {macro_zone} is owned by player {anchor_player} (not {caller_player_id})"
                ));
            }
            let (x, y) = unpack_xy(xy);
            let micro = if surface == MINI_ZONE_LAYER {
                // Hex anchor: cell `(q, r)`, snapped to centre (the renderer
                // ignores the offset under `SNAP_HEX`). Mirror of the world
                // drop arm above + the client's `looseKindForSurface`.
                Micro::Loose { local_q: q, local_r: r, x, y, kind: SNAP_HEX }
            } else {
                // Pocket dimension: rect interior, no cell — pure `(x, y)`.
                Micro::Loose { local_q: 0, local_r: 0, x, y, kind: LOOSE_RECT }
            };
            Ok((surface, macro_zone, micro))
        }
        other => Err(format!(
            "place_card: unsupported surface {other} (expected INVENTORY, PLAYER_INVENTORY, POCKET_DIMENSION, MINI_ZONE, or WORLD_LAYER+)"
        )),
    }
}
