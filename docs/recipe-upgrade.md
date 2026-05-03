# Recipe Upgrade System

## Problem

Earlier the server cancelled every action holding any submitted card and
re-ran the matcher from scratch. A card already running a recipe got
cancelled even when the new stack arrangement matched the same recipe —
or a worse one. A no-op stack submission reset the action timer.

We want to:

- Keep the existing action running when the same recipe still applies.
- Switch to a strictly better recipe when one is now available.
- Cancel only when the recipe is no longer satisfied at all.

## The model

Stacks reach the server as branch chains. For a top-stack submission
`{ root, stack_up: [A, B, C, D], stack_down: [...] }` the top-branch
chain is `[root, A, B, C, D]`. The submitted `root` is the chain root
for matching purposes regardless of which branch we're walking, so
`recipe.root` resolves against `chain[0]` for both top and bottom
branches.

The matcher walks the branch chain and treats every card in it as a
**potential actor**. For each actor candidate it builds a *visible
chain* outward from that card, scores all recipes of the relevant
type against that window, and applies the upgrade rules below.

### Visible chain

Walk outward from the actor (toward higher branch indices). Include a
card if it is **free** (no `CardHold`) or **claimed by the actor's own
current action**. Stop at the first card claimed by some other action
(exclude it).

This is what gives running actions their "exclusion zone" — cards
inside another action's claim aren't visible to a new actor candidate.
It also lets an action see its own slot fillers when the matcher
re-evaluates it.

Recipes whose slot count exceeds the visible window length are skipped.

### Per-actor scoring

For each candidate, score every recipe of the relevant `RecipeType`
against the visible window. The lex-ordered `MatchWeight {
tile_weight, root_weight, slot_weight }` picks the winner; ties go to
declaration order. See [`data/recipes/AGENT.md`](../../data/recipes/AGENT.md)
for the per-leaf weights and tier ordering.

The chain root used for `recipe.root` matching is always
`branch_chain[0]` — the submitted root — not the actor itself. The
window only constrains where the slot list sits.

### Branch-type isolation

The chain root sits at `chain[0]` of *both* the top and bottom branches.
That same card may be the actor of (say) a `TopStack` action while we're
processing the bottom branch. The bottom-branch evaluator must **not**
re-decide that `TopStack` action — when the actor's current action is a
different `RecipeType` than the branch we're processing, leave it
alone. The other branch's iteration will handle it.

Without this guard, processing a Y-stack's bottom branch could cancel
the actor's `TopStack` action just because a `BottomStack` recipe also
fits the same actor — a unilateral cross-branch override that breaks
the "one action per actor" invariant.

### Upgrade decision

With the actor's current action (`current`, may be `None`) and the
best-scoring recipe over its visible window (`best`, may be `None`):

```
(None,    None)    → nothing
(Some(a), None)    → cancel a
(None,    Some(r)) → start r
(Some(a), Some(r)) →
    same recipe AND slot fillers unchanged → keep a running
    otherwise → cancel a, start r
```

**Slot fillers are strict.** Compared as a set, the cards in
`branch_chain[actor_idx..actor_idx+slot_count]` must equal the action's
currently-claimed cards. Any swap, removal, or replacement cancels the
action — even when the same recipe ID still matches the new arrangement,
because the *identity* of the slot fillers is part of what's running.

**The chain root is fluid and unheld.** The chain root is **not** in
`CardHold` and is **not** stored on the `Action` row. Holding the root
would make it a contention point: a single `human` card couldn't be
the root of `[attack, sword]` over the top branch and `[heal, anima]`
over the bottom at the same time. By leaving the root unheld, multiple
recipes can share it.

This means a different chain root that still satisfies `recipe.root`
is invisible to the action — the matcher just re-validates `root`
against the current `branch_chain[0]` on every upgrade pass. If the
new root no longer matches, the score returns `None` and the action
is cancelled. If it still matches, the action keeps running unchanged.
No bookkeeping update is needed for a root swap.

### What's claimed (and what's not)

The `CardHold` rows for an action enumerate exactly **actor + slot
fillers**. The chain root, even when `recipe.root` is set, is not
held. Consequences:

- Reagent index `0` (consume the chain root) is currently a no-op for
  stack recipes — the chain root isn't recoverable from server state at
  completion time. `OnCreate` recipes have actor == root, so reagent
  `0` consumes the actor card as expected. None of the recipes in
  `data/recipes/01.json` use reagent `0` for stack types.
- `RootPanel` product target falls back to the actor's panel for stack
  recipes — same reason. In the inventory POC every claimed card is
  in the same player's panel, so `RootPanel` and `ActorPanel` resolve
  to the same destination. When world layers land and a stack can
  span panels, the chain root will need a server-side representation
  (likely passed at submission and snapshotted onto the `Action`).

## Triggers

- **Stack submission** (`submit_inventory_stacks`) calls
  `process_top_branch` and `process_bottom_branch` for every submitted
  stack. The blanket pre-cancel is gone — cancellation now happens
  inside the upgrade decision, only for actions that are actually
  disturbed. A no-op submission leaves running actions running with
  their timers untouched.
- **Card insertion** (`insert_card_row`) calls
  `try_start_on_create_action`. `OnCreate` recipes use the new card as
  both root and actor; the visible chain is the card itself. No upgrade
  logic is needed (an `OnCreate` candidate either runs or doesn't —
  there's no slot window to rearrange).

## Defense in depth at completion

Before `complete_action` generates products and consumes reagents, it
calls `recipe_still_satisfies_claim`:

- Every claimed card must still match at least one slot entity in the
  recipe (or `recipe.root` for `OnCreate`, where actor == root).

The chain root for stack recipes isn't checked here — it isn't held by
the action and isn't recoverable from server state at completion. The
root entity is re-validated by the matcher on every upgrade pass; if
the root drifted away, the upgrade decision cancels the action there.

The upgrade machinery is supposed to have cancelled any drifted action
long before completion, so this check is belt-and-braces — not the
primary defense. If it ever fires in practice, that's the smoking gun
for an upgrade-path bug; the action is torn down rather than producing
mismatched output.

## Implementation map

| Concern | Location |
| --- | --- |
| `Action` table (no `root_card_id` — root isn't tracked) | [`actions.rs`](../server/spacetimedb/src/actions.rs) — `Action` struct |
| Visible-chain walk | `actions.rs` — `build_visible_chain` |
| Per-actor recipe scoring | `actions.rs` — `score_recipe_for_actor` |
| Strict slot-filler check | `actions.rs` — `slot_fillers_unchanged` |
| Four-way upgrade decision | `actions.rs` — `process_actor_candidate` |
| Branch driver | `actions.rs` — `process_branch`, plus `process_top_branch` / `process_bottom_branch` public entry points |
| Defense at completion | `actions.rs` — `recipe_still_satisfies_claim` |
| Caller integration | [`cards.rs`](../server/spacetimedb/src/cards.rs) — `submit_inventory_stacks` |
