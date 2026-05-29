//! Soul-stack walk — the one piece of the legacy recipe evaluator that
//! still has callers outside the recipe pipeline.
//!
//! Previously this module hosted `aspect_pool`, `entity_satisfied_pool`,
//! `entity_specificity`, `resolve_has`, `has_specificity_bonus`,
//! `has_predicates_feasible`, and the `HasMatches` / `HasCandidates` /
//! `RoleMatches` types — all bound to the `Entity` / `HasOps` /
//! `RecipeDef` types of the typed recipe model. The tape-form rewrite
//! collapses those: predicates are evaluated by the new server-side
//! verifier (`actions::propose_action`) walking the recipe's input
//! statements with iterator offsets pre-bound by the client, and stock
//! / inventory effects are applied by the new tape walker
//! (`action_completion::apply`).
//!
//! [`soul_stack`] survives because `utilities` uses it for queue-walk
//! traversal that's orthogonal to recipe evaluation — it just collects
//! every alive card resting on one branch of a soul's stack.

use std::collections::BTreeSet;

use spacetimedb::ReducerContext;

use crate::cards::{self, cards as _cards_table, Card};
use crate::flags::state_flags;

/// Max depth of the soul-stack walk. Bounds pathological chains and
/// keeps traversal O(1) in chain length under normal gameplay. The
/// "top stack = equipment" convention typically tops out at a handful
/// of cards; 16 leaves comfortable slack.
const SOUL_STACK_MAX_DEPTH: usize = 16;

/// Walk one branch of a soul card's stack and return every alive card
/// resting on it, ordered breadth-first from the soul outward (so the
/// immediate root-stacked child appears first, its child second, etc.).
///
/// `direction` is `STACK_DIR_UP` or `STACK_DIR_DOWN` — the recipe's
/// stack-direction convention: things on top of the soul go UP,
/// queued actions / debuffs hang DOWN. Both branches are valid
/// parent-pointer chains; this function picks one and ignores the
/// other.
///
/// Implementation: scan cards whose `owner_id` is the soul (under the
/// card-owner model, every inventory card carrying this soul has
/// `owner_id == soul_card_id`), build a parent → children map keyed on
/// `micro_location`, then BFS from `soul_card_id` with a depth cap.
/// Cards with `dead` or `slot_hold` set are excluded — slot-held cards
/// are claimed by an in-flight recipe and shouldn't be re-bound to
/// another action concurrently. Cards not in a chain state (`Free`)
/// are excluded — only `OnRoot` (immediate stacked-on-root
/// child) and `Slot` (parent-pointer chain) qualify as equipment /
/// action-stack positions.
pub fn soul_stack(
    ctx: &ReducerContext,
    soul_card_id: u32,
    direction: u8,
) -> Vec<Card> {
    if soul_card_id == 0 {
        return Vec::new();
    }
    // Flat-root: every card stacked on the soul points at it directly via
    // `micro_location`. One btree lookup gets the members; filter to the
    // requested branch and sort by `stack_index` for chain order (index 0 =
    // closest to root). No BFS/parent-walk needed.
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut out: Vec<Card> = Vec::new();
    for row in ctx.db.cards().micro_location().filter(soul_card_id) {
        if !seen.insert(row.card_id) {
            continue;
        }
        let Some(latest) = cards::latest(ctx, row.card_id) else {
            continue;
        };
        if latest.flags_state & state_flags().dead != 0
            || cards::slot_hold_count(latest.flags_bk) > 0
        {
            continue;
        }
        if !cards::micro_is_card(&latest) || latest.micro_location != soul_card_id {
            continue;
        }
        if cards::stack_branch(&latest) != direction {
            continue;
        }
        out.push(latest);
    }
    out.sort_by_key(cards::stack_index);
    out
}
