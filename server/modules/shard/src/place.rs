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

use crate::cards::{self, cards as _cards_table};
use crate::flags::state_flags;
use crate::packed::PLAYER_DIMENSION_LAYER;
use crate::packed::PLAYER_INVENTORY_LAYER;
use crate::packed::{
    pack_micro_zone, pack_slot_micro_zone, pack_stack_micro_zone, unpack_micro_zone,
    StackedState, INVENTORY_LAYER, MINI_ZONE_LAYER, POCKET_DIMENSION_LAYER, STACK_DIR_DOWN,
    STACK_DIR_HEX, STACK_DIR_UP, WORLD_LAYER,
};
use crate::players;

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
    pub macro_zone: u32,
    pub q: u8,
    pub r: u8,
    pub xy: u32,
}

const PLACEMENT_STACK: u8 = 0;
const PLACEMENT_LOOSE: u8 = 1;

/// Depth cap for ancestor walks (cycle detection, chain-root resolve,
/// descendant-restamp). Matches `cards::OWNER_WALK_DEPTH_CAP`'s
/// 32-hop slack; realistic chains top out around 5.
const PLACE_WALK_DEPTH_CAP: usize = 32;

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
/// current state (same surface / macro_zone / micro_zone /
/// micro_location), the write still fires but is a no-op semantically
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

    // Collect descendants up-front so the same set is validated and
    // re-stamped. Walks via `owner_id` btree (mirrors `unequip_card`
    // line ~360 in `utilities.rs`); for sources outside an inventory
    // bucket (world-loose with no soul), this returns empty and the
    // single-card move still succeeds.
    let descendants = collect_descendants(ctx, &source, now_ms);
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
    // (surface, macro_zone, micro_zone, micro_location). `owner_id`
    // is intentionally not part of the destination tuple —
    // ownership is an independent property of a card (controls who
    // may move it), unchanged by placement. Cards that need a
    // different owner go through an explicit ownership-transfer
    // reducer (TODO; today no such reducer exists, so cards keep
    // whatever owner they were created with).
    let (new_surface, new_macro_zone, new_micro_zone, new_micro_location) =
        match placement.kind {
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

    // Source write. `owner_id` is intentionally untouched.
    cards::update_with_at(ctx, card_id, now_ms, |c| {
        c.surface = new_surface;
        c.macro_zone = new_macro_zone;
        c.micro_zone = new_micro_zone;
        c.micro_location = new_micro_location;
    });

    // Re-stamp descendants' ambient surface + macro_zone so they
    // travel with the chain root. `micro_zone` / `micro_location`
    // (parent pointers within the sub-chain) and `owner_id`
    // (independent of position) stay intact.
    for d in &descendants {
        cards::update_with_at(ctx, d.card_id, now_ms, |c| {
            c.surface = new_surface;
            c.macro_zone = new_macro_zone;
        });
    }

    Ok(())
}

/// Resolve a `Stack` placement to the concrete `(surface, macro_zone,
/// micro_zone, micro_location)` write tuple.
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
) -> Result<(u8, u32, u8, u32), String> {
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

    // Cycle check: source must not be an ancestor of parent. Walk
    // parent's `micro_location` chain upward and reject if `source_id`
    // appears. Caps at `PLACE_WALK_DEPTH_CAP` against malformed chains.
    let mut walker_id = parent_id;
    for _ in 0..PLACE_WALK_DEPTH_CAP {
        if walker_id == source_id {
            return Err(format!(
                "place_card: stacking would form a cycle (source {source_id} is an ancestor of parent {parent_id})"
            ));
        }
        let Some(row) = cards::prior_at(ctx, walker_id, now_ms) else {
            break;
        };
        if row.micro_location == 0 {
            break;
        }
        walker_id = row.micro_location;
    }

    // Ownership: the chain rooted at parent must be caller-owned. Use
    // `owning_player`'s existing walk — terminates at the soul (which
    // carries `is_owned_by_player`) or world (`owner_id == 0` → WORLD_PLAYER_ID).
    // World-rooted chains (e.g. a tile-card the player is targeting)
    // pass; per-hex permission isn't modeled at this layer.
    let chain_player =
        cards::owning_player(ctx, parent_id).unwrap_or(cards::WORLD_PLAYER_ID);
    if chain_player != cards::WORLD_PLAYER_ID && chain_player != caller_player_id {
        return Err(format!(
            "place_card: parent {parent_id}'s chain is owned by player {chain_player} (not {caller_player_id})"
        ));
    }

    // Find the current top of the branch in `direction` off parent.
    // BFS via `owner_id` btree mirrors `equip_card`'s `soul_stack`
    // call; here we filter on direction matching the branch and
    // walking `micro_location` back to `parent_id` or its ancestors
    // in that branch.
    let top_id = walk_branch_top(ctx, parent_id, direction, now_ms);

    let (immediate_parent_id, new_micro_zone) = match top_id {
        Some(top) => (top, pack_slot_micro_zone(direction)),
        None => (
            parent_id,
            // First child in this branch: `OnRoot` with position=1.
            pack_stack_micro_zone(1, direction, StackedState::OnRoot),
        ),
    };

    Ok((
        parent.surface,
        parent.macro_zone,
        new_micro_zone,
        immediate_parent_id,
    ))
}

/// Walk the chain rooted at `parent_id` in `direction` and return the
/// current top's card_id. Mirrors `recipe_eval::soul_stack`'s shape
/// but starts from an arbitrary card and a specific direction.
/// Returns `None` when no child exists in that branch.
fn walk_branch_top(
    ctx: &ReducerContext,
    parent_id: u32,
    direction: u8,
    now_ms: u64,
) -> Option<u32> {
    use crate::packed::micro_zone_direction;
    use std::collections::BTreeSet;

    // We want children whose `micro_location` walks back to
    // `parent_id` in `direction`. Iterate every card row at the
    // parent's surface/macro_zone via the owner_id index; dedupe by
    // card_id; pick the deepest-position child.
    let parent = cards::prior_at(ctx, parent_id, now_ms)?;
    let chain_owner = chain_root_id(ctx, parent_id, now_ms).unwrap_or(parent_id);

    let mut seen: BTreeSet<u32> = BTreeSet::new();
    // Map immediate-parent → child for direction-matching state-Slot
    // and direction-matching state-OnRoot.
    let mut children_of: std::collections::BTreeMap<u32, Vec<u32>> =
        std::collections::BTreeMap::new();
    for row in ctx.db.cards().owner_id().filter(chain_owner) {
        if !seen.insert(row.card_id) {
            continue;
        }
        let Some(latest) = cards::prior_at(ctx, row.card_id, now_ms) else {
            continue;
        };
        if latest.surface != parent.surface || latest.macro_zone != parent.macro_zone {
            continue;
        }
        let (_, _, state) = unpack_micro_zone(latest.micro_zone);
        if !matches!(state, StackedState::OnRoot | StackedState::Slot) {
            continue;
        }
        if micro_zone_direction(latest.micro_zone) != direction {
            continue;
        }
        children_of
            .entry(latest.micro_location)
            .or_default()
            .push(latest.card_id);
    }

    // Walk: parent → child → grandchild → ... until we hit a node
    // with no children. That node is the top of the branch.
    let mut cur = parent_id;
    for _ in 0..PLACE_WALK_DEPTH_CAP {
        match children_of.get(&cur).and_then(|v| v.first().copied()) {
            Some(next) => cur = next,
            None => break,
        }
    }
    if cur == parent_id {
        None
    } else {
        Some(cur)
    }
}

/// Walk a card's chain up via `micro_location` until reaching a Free
/// card (the chain root). Returns the root's `card_id`, or `None`
/// when the walk dead-ends (`micro_location = 0` on a non-Free card)
/// or exceeds the depth cap.
fn chain_root_id(ctx: &ReducerContext, card_id: u32, time_ms: u64) -> Option<u32> {
    let mut cur = card_id;
    for _ in 0..PLACE_WALK_DEPTH_CAP {
        let row = cards::prior_at(ctx, cur, time_ms)?;
        let (_, _, state) = unpack_micro_zone(row.micro_zone);
        if matches!(state, StackedState::Free) {
            return Some(cur);
        }
        if row.micro_location == 0 {
            return None;
        }
        cur = row.micro_location;
    }
    None
}

/// Resolve a `Loose` placement to the concrete write tuple.
/// `owner_id` is not part of the return — placement doesn't change
/// who owns the card.
fn resolve_loose_target(
    ctx: &ReducerContext,
    caller_player_id: u32,
    surface: u8,
    macro_zone: u32,
    q: u8,
    r: u8,
    xy: u32,
) -> Result<(u8, u32, u8, u32), String> {
    match surface {
        INVENTORY_LAYER => {
            // macro_zone is the soul's card_id; the soul must belong to caller.
            let soul_player = cards::owning_player(ctx, macro_zone).unwrap_or(cards::WORLD_PLAYER_ID);
            if soul_player != caller_player_id {
                return Err(format!(
                    "place_card: inventory target soul {macro_zone} is owned by player {soul_player} (not {caller_player_id})"
                ));
            }
            Ok((
                INVENTORY_LAYER,
                macro_zone,
                pack_micro_zone(0, 0, StackedState::Free),
                xy,
            ))
        }
        PLAYER_INVENTORY_LAYER => {
            // macro_zone IS the player_id directly — must match the
            // caller to prevent dropping into another player's bag.
            if macro_zone != caller_player_id {
                return Err(format!(
                    "place_card: player-inventory target {macro_zone} is not the caller's player_id ({caller_player_id})"
                ));
            }
            Ok((
                PLAYER_INVENTORY_LAYER,
                macro_zone,
                pack_micro_zone(0, 0, StackedState::Free),
                xy,
            ))
        }
        PLAYER_DIMENSION_LAYER => {
            // Player-dim placement: `macro_zone` is the packed chunk
            // coord (same encoding as world). `(q, r)` is the local
            // hex within the chunk. We trust the dim's owner_id
            // discriminator to scope the placement — the card
            // keeps its existing `owner_id` through `place_card`, so
            // dropping a card you don't own into another player's
            // dim address would just leave it owner-mismatched and
            // the dim subscription would skip it client-side.
            // (Future hardening: explicitly require the card's
            // current `owner_id == caller_player_id` here too.)
            if q >= 8 || r >= 8 {
                return Err(format!(
                    "place_card: player-dim target ({q}, {r}) out of range (0..=7 each)"
                ));
            }
            Ok((
                PLAYER_DIMENSION_LAYER,
                macro_zone,
                pack_micro_zone(q, r, StackedState::Free),
                0,
            ))
        }
        s if s >= WORLD_LAYER => {
            if q >= 8 || r >= 8 {
                return Err(format!(
                    "place_card: world target ({q}, {r}) out of range (0..=7 each)"
                ));
            }
            Ok((
                surface,
                macro_zone,
                pack_micro_zone(q, r, StackedState::Free),
                0,
            ))
        }
        MINI_ZONE_LAYER | POCKET_DIMENSION_LAYER => {
            // Mini-zone / pocket-dimension placement: `macro_zone` is
            // the anchor card's `card_id`. The anchor must be reachable
            // from the caller — for simplicity require caller-owned
            // anchor (same gate as inventory). Mini-zone has internal
            // hex coords; pocket-dimension typically uses `xy`.
            let anchor_player =
                cards::owning_player(ctx, macro_zone).unwrap_or(cards::WORLD_PLAYER_ID);
            if anchor_player != caller_player_id {
                return Err(format!(
                    "place_card: container anchor {macro_zone} is owned by player {anchor_player} (not {caller_player_id})"
                ));
            }
            let micro_zone = if surface == MINI_ZONE_LAYER {
                pack_micro_zone(q, r, StackedState::Free)
            } else {
                pack_micro_zone(0, 0, StackedState::Free)
            };
            let micro_location = if surface == MINI_ZONE_LAYER { 0 } else { xy };
            Ok((surface, macro_zone, micro_zone, micro_location))
        }
        other => Err(format!(
            "place_card: unsupported surface {other} (expected INVENTORY, PLAYER_INVENTORY, POCKET_DIMENSION, PLAYER_DIMENSION, MINI_ZONE, or WORLD_LAYER+)"
        )),
    }
}

/// Collect every card whose chain-of-parents (`micro_location`) walks
/// back through `source`. The source itself is NOT included. Used
/// to validate sub-chain holds and to re-stamp surface / macro_zone
/// after the move so descendants travel with the chain root.
/// `owner_id` is preserved per descendant — placement doesn't change
/// who owns the card.
///
/// Mirrors `utilities::chain_descendants`'s shape but indexed via the
/// source's current `owner_id` (chain root). World-rooted chains
/// (`owner_id == 0`) return empty — descendant tracking is only
/// meaningful inside an owner-card bucket.
fn collect_descendants(
    ctx: &ReducerContext,
    source: &cards::Card,
    now_ms: u64,
) -> Vec<cards::Card> {
    use std::collections::{BTreeMap, BTreeSet};

    if source.owner_id == 0 {
        return Vec::new();
    }

    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut children_of: BTreeMap<u32, Vec<cards::Card>> = BTreeMap::new();
    for row in ctx.db.cards().owner_id().filter(source.owner_id) {
        if !seen.insert(row.card_id) {
            continue;
        }
        let Some(latest) = cards::prior_at(ctx, row.card_id, now_ms) else {
            continue;
        };
        if latest.micro_location == 0 {
            continue;
        }
        children_of
            .entry(latest.micro_location)
            .or_default()
            .push(latest);
    }

    let mut out: Vec<cards::Card> = Vec::new();
    let mut frontier: Vec<u32> = vec![source.card_id];
    for _ in 0..PLACE_WALK_DEPTH_CAP {
        let mut next: Vec<u32> = Vec::new();
        for parent in &frontier {
            if let Some(children) = children_of.remove(parent) {
                for child in children {
                    next.push(child.card_id);
                    out.push(child);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}
