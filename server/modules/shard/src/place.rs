//! `place_card` — the unified card-placement reducer.
//!
//! "Put this card at this position" — a stack onto any parent card (in any
//! branch direction), or a loose placement at an explicit address. The
//! *validation + resolution* (eligibility, cycle/ownership checks, branch
//! indexing, member re-root) is the shared, sans-IO
//! [`resonantdust_state::stack`] model — the SAME `plan_place` the client runs to
//! predict the move, so the authority and the prediction can't drift. This
//! reducer is just the IO shell: adapt the db to a [`StackStore`], run
//! `plan_place`, apply the resulting writes.
//!
//! See `docs/PLACE_CARD_GENERALIZATION.md`.

use spacetimedb::{reducer, ReducerContext, SpacetimeType};

use crate::cards::{self, cards as _cards_table, MicroPlace};
use resonantdust_state::recipe_state::{CardStore, CardView};
use resonantdust_state::stack::{self, plan_place, StackStore};

/// Unpack a wire `xy` u32 (`[x:i16 | y:i16]`) — the loose within-cell offset the
/// client packs into `Placement.xy`. Clamped to the i12 range the loose
/// `micro_location` layout stores (±2047).
fn unpack_xy(xy: u32) -> (i16, i16) {
    let x = (xy >> 16) as u16 as i16;
    let y = xy as u16 as i16;
    (x.clamp(-2048, 2047), y.clamp(-2048, 2047))
}

/// Where the placement puts the source card. `kind = 0` → Stack (uses
/// `parent_id` / `direction`); `kind = 1` → Loose (uses `surface` / `macro_zone`
/// / `q` / `r` / `xy`). Flat-struct (not a Rust enum) keeps the wire format
/// stable across SpacetimeDB schema migrations; mapped to [`stack::Placement`]
/// internally.
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

/// Place a card at a caller-specified position. Validates the move + resolves the
/// destination via the shared [`stack::plan_place`], then applies the write plan
/// (source + members). `owner_id` is never touched — placement is independent of
/// ownership. Idempotent: a no-op re-place lands a clean dirty-diff row.
#[reducer]
pub fn place_card(
    ctx: &ReducerContext,
    client_time_ms: u64,
    caller_player_id: u32,
    card_id: u32,
    placement: Placement,
) -> Result<(), String> {
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;
    let plan = plan_place(
        &Db(ctx),
        card_id,
        to_stack_placement(placement)?,
        caller_player_id,
        now_ms,
        // The shard is content-agnostic (no bundle): a Stack placement only ever
        // stacks non-tile card rows, which carry the regular-card default bits.
        &|_| resonantdust_codec::stacking::DEFAULT_BITS,
    )?;
    // Apply the plan. `surface` is folded into `macro_zone` already (the plan's
    // `macro_zone` is the full key); `micro.place` writes `micro_location` + the
    // placement flag bits together, preserving the card's other flags.
    for w in plan.writes {
        cards::update_with_at(ctx, w.card_id, now_ms, |c| {
            c.macro_zone = w.macro_zone;
            w.micro.place(c);
        });
    }
    Ok(())
}

/// Batch position sync — write N already-resolved card positions in ONE
/// transaction (one coalesced row per card @`now_ms`, so the whole batch fans out
/// as a single commit). This is the **commit-based** position path: the client
/// keeps position client-local while a zone has ≤1 observer, then flushes the
/// dirty set here (before a recipe proposal, on an observer 1→>1 transition, or
/// any forced sync). Positions are written **verbatim** — the client already ran
/// the shared `plan_place` locally, so its resolved `(macro_zone,
/// micro_location, stack_state)` per card IS the truth; re-resolving server-side
/// could only drift from it. Parallel arrays, indexed together; `stack_state` is
/// the u8 `[stack_id:u4 | stack_index:u4]` (bits 0-7 of `flags`; `stack_id==0` ⇒
/// loose). Trusts its args (gate-authority posture); `caller_player_id` is kept
/// for future Permissions.
#[reducer]
pub fn move_cards(
    ctx: &ReducerContext,
    client_time_ms: u64,
    // SpacetimeDB `/call` keys on the EXACT Rust param name, so this must match the
    // client's `caller_player_id` arg (no leading underscore). Kept for future
    // Permissions; unused today.
    caller_player_id: u32,
    card_ids: Vec<u32>,
    macro_zones: Vec<u64>,
    micro_locations: Vec<u32>,
    stack_states: Vec<u8>,
) -> Result<(), String> {
    let _ = caller_player_id;
    let n = card_ids.len();
    if macro_zones.len() != n || micro_locations.len() != n || stack_states.len() != n {
        return Err("move_cards: parallel-array length mismatch".to_string());
    }
    let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;
    for i in 0..n {
        // Decode the verbatim placement from (micro_location, stack_state-as-flags)
        // — `Micro::of` reads stack_id/index from the low byte — then `place` it,
        // preserving the card's other flags (state + holds).
        let micro = cards::Micro::of(micro_locations[i], stack_states[i] as u32);
        cards::update_with_at(ctx, card_ids[i], now_ms, |c| {
            c.macro_zone = macro_zones[i];
            micro.place(c);
        })
        .ok_or_else(|| format!("move_cards: card {} not found at {now_ms}", card_ids[i]))?;
    }
    Ok(())
}

/// Map the wire [`Placement`] to the shared [`stack::Placement`] enum.
fn to_stack_placement(p: Placement) -> Result<stack::Placement, String> {
    match p.kind {
        PLACEMENT_STACK => Ok(stack::Placement::Stack {
            parent_id: p.parent_id,
            direction: p.direction,
        }),
        PLACEMENT_LOOSE => {
            let (x, y) = unpack_xy(p.xy);
            Ok(stack::Placement::Loose {
                surface: p.surface,
                macro_zone: p.macro_zone,
                q: p.q,
                r: p.r,
                x,
                y,
            })
        }
        other => Err(format!(
            "place_card: unknown placement kind {other} (expected 0=Stack or 1=Loose)"
        )),
    }
}

/// [`StackStore`] over the live db — the IO half the shared model abstracts over.
struct Db<'a>(&'a ReducerContext);

fn to_view(c: cards::Card) -> CardView {
    CardView {
        card_id: c.card_id,
        owner_id: c.owner_id,
        micro_location: c.micro_location,
        macro_zone: c.macro_zone,
        packed_definition: c.packed_definition,
        flags: c.flags,
        stock: c.stock,
    }
}

impl CardStore for Db<'_> {
    fn card_at(&self, card_id: u32, time_ms: u64) -> Option<CardView> {
        cards::prior_at(self.0, card_id, time_ms).map(to_view)
    }
}

impl StackStore for Db<'_> {
    /// Every current stack member of `root_id` (flat: `micro_location == root_id`
    /// AND `micro_is_card`). Single `micro_location` btree lookup, deduped by
    /// card_id at the latest row.
    fn members_of(&self, root_id: u32, now_ms: u64) -> Vec<CardView> {
        use std::collections::BTreeSet;
        let ctx = self.0;
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        let mut out: Vec<CardView> = Vec::new();
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
            out.push(to_view(latest));
        }
        out
    }
}
