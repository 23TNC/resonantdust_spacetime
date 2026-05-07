# Resonant Dust — SpacetimeDB Module

This is the authoritative server module. It owns the canonical state of
every card, player, world tile, and in-progress action. Clients subscribe
to the public tables here to render their inventories and the world; clients
call reducers here to request state changes.

For language/SDK rules see [../AGENTS.md](../AGENTS.md). This file documents
the **game model** encoded by the tables and reducers in `src/`.

The world board, action machinery, and magnetic slot-fill loop are all
live; the inventory POC has grown into a full inventory-↔-world card
system. What's still TODO is auth (`claim_or_login` is trust-on-first-use)
and pretty much everything except the despair-recipe path inside the
magnetic system.

---

## Table layout summary

Public tables (clients can subscribe):

| Table | Key | Purpose |
| --- | --- | --- |
| `cards` | `card_id` | Every card in the world or in someone's inventory. |
| `players` | `player_id` | Persistent player record (name, soul placement). |
| `actions` | `action_id` | One row per in-progress recipe action. |
| `magnetic_actions` | `magnetic_action_id` | Slot-fill phase for magnetic recipes (precedes `actions`). |
| `zones` | `zone_id` | Bulk world-tile storage — one row per 8×8 chunk. |

Private tables (server-only bookkeeping):

| Table | Purpose |
| --- | --- |
| `player_sessions` | Maps current `Identity` → stable `player_id`. |
| `card_holds` | Claims a card to an action. PK on `card_id` enforces "one action per card." |
| `action_scheduler` | Drives `complete_action` when an action's duration elapses. |
| `magnetic_inputs` | Drives `magnetic_tick` for the slot-fill loop. |
| `pending_card_deletions` | Scheduled reaper queue — fires `reap_dead_card` after a card's death animation has played out. |
| `pending_action_deletions` | Same for actions. |
| `pending_magnetic_action_deletions` | Same for magnetic actions. |

Every public-table row carries a `delta_t: u8` column that reports the
scheduled-reducer lag at the time of the write — see the
[delta_t section](#scheduled-reducer-lag-delta_t) below.

---

## Card storage model

A card is a single row in the `cards` table:

| Column              | Type | Meaning                                                                  |
| ------------------- | ---- | ------------------------------------------------------------------------ |
| `card_id`           | u32  | Primary key. Auto-increment. Unique across all cards.                    |
| `layer`             | u8   | Where the card lives. `LAYER_INVENTORY = 1`; `LAYER_WORLD = 64`. Future layers (dream / underworld / …) take values `>= LAYER_WORLD`. |
| `macro_zone`        | u32  | Inventory cards: holder's `player_id`. World cards: packed `(zone_q:i16, zone_r:i16)` axial chunk coords (see [`packing::pack_world_macro_zone`](src/packing.rs)). Subscription discriminator either way. Indexed. |
| `micro_zone`        | u8   | World cards: `[local_q:u3][local_r:u3][stack_state:u2]` — in-zone hex coords plus the card's stack role (bits 1..0). Inventory cards: held at 0. The high 3 bits also serve as the client's "trust this position" signal — `(micro_zone & MICRO_ZONE_LOCAL_Q_MASK) != 0` tells the client the server is forcing a placement; `insert_card_row` zeroes both `micro_zone` and `micro_location` so inventory inserts default to "client owns layout," and `insert_card_row_at_position` is the explicit opt-in for the rare server-forces-placement case. |
| `micro_location`    | u32  | Variant per `micro_zone.stack_state`: parent `card_id` for stacked cards (states 1–3) or packed `(i16 x, i16 y)` for loose world cards. Inventory cards: 0. Indexed (btree) so `magnetic::find_attached` can resolve a card's children in O(matches + log n). |
| `owner_id`          | u32  | Player who owns this card. Not necessarily the same as the inventory holder. `0` is the "world-owned" sentinel for cards belonging to no player. Indexed. |
| `packed_definition` | u16  | `[card_type:u4][card_category:u4][definition_id:u8]`. Card identity for the renderer/rules. |
| `flags`             | u8   | Bit flags for per-card state. Currently: `FLAG_CARD_POSITION_HOLD = 1<<0`, `FLAG_CARD_DROP_HOLD = 1<<3`, `FLAG_CARD_DEAD = 1<<7`. The `position_locked` (bit 1) and `drop_locked` (bit 4) variants are reserved for "permanent lock — user can never pick up" but aren't enforced server-side yet. |
| `delta_t`           | u8   | Scheduled-reducer lag at the time of this write, in 32-ms increments. See [`delta_t.rs`](src/delta_t.rs). |

### Card death

When a recipe consumes a card (or a player is deleted), the card
**isn't deleted directly** — it's flagged dead. `mark_card_dead`:

1. Sets `flags |= FLAG_CARD_DEAD` (bit 7) and stamps `delta_t`.
2. Schedules a `pending_card_deletion` row for `now + CARD_REAP_DELAY_SECS`
   (10 seconds).
3. The scheduled reducer `reap_dead_card` fires later and runs the actual
   `ctx.db.cards().delete(...)`.

The two-stage delete exists so the client can play a death animation
back-dated by `32 * delta_t` ms. The same pattern is used by `Action`
and `MagneticAction` rows (`mark_action_dead` / `mark_magnetic_action_dead`)
— they also carry `dead` (bit 7), `canceled` (bit 1), and `complete`
(bit 2) flags so the client can distinguish "ended successfully" from
"canceled mid-flight."

---

## Subscription model

Clients receive data by subscribing to public tables. They scope those
subscriptions by `(layer, macro_zone)`:

- **Inventory**: `layer == LAYER_INVENTORY AND macro_zone == own_player_id`.
- **World zones around viewport / player**: a union of
  `layer == LAYER_WORLD AND macro_zone == pack_world_macro_zone(zoneQ, zoneR)`
  for every chunk the client wants visible.

The same approach applies to `actions` and `magnetic_actions`: subscribe
on `macro_zone` matching the player's id (for inventory-anchored actions)
or matching world zone coordinates (for world-anchored actions).

`zones` is subscribed by world chunk too — its `macro_zone` matches the
chunks of any world cards sitting in it.

---

## Players

`Identity` is treated as **ephemeral** — a player who reconnects (or signs in
fresh) generally arrives with a new `Identity`. So `Player` is keyed by a
stable name, and a separate session table maps the current connection's
identity to the player's `player_id`.

`players` (public) — persistent player record, referenced by
`cards.macro_zone` and elsewhere:

| Column            | Type   | Notes                                                                                                   |
| ----------------- | ------ | ------------------------------------------------------------------------------------------------------- |
| `player_id`       | u32    | Primary key. Auto-increment.                                                                            |
| `name`            | String | Display name. Casing preserved. Bounded by `MAX_PLAYER_NAME_LEN` (64 bytes); use `validate_player_name`. |
| `name_normalized` | String | Unique. Lowercased mirror of `name` for case-insensitive uniqueness. |
| `layer`           | u8     | Layer the player's soul currently occupies (world layer). `0` while unplaced. |
| `macro_zone`      | u32    | World macro_zone the soul currently occupies. `0` while unplaced. Indexed. |
| `micro_zone`      | u8     | In-zone position of the soul: `[local_q:u3][local_r:u3][stack_state:u2]`. `0` while unplaced. |
| `micro_location`  | u32    | Within-`micro_zone` position of the soul. Same variant rules as `Card.micro_location`. |
| `delta_t`         | u8     | Scheduled-reducer lag at write time. |

`player_sessions` (private) — the bridge between an active connection's
`Identity` and the player's `player_id`:

| Column      | Type     | Notes                                                                  |
| ----------- | -------- | ---------------------------------------------------------------------- |
| `identity`  | Identity | Primary key. The caller's current connection identity.                 |
| `player_id` | u32      | The stable player this session belongs to. Indexed for cleanup queries. |

`claim_or_login(name)` creates the session (and the `Player` row if the
name is new). The `client_disconnected` lifecycle reducer in
[`src/players.rs`](src/players.rs) removes the row on disconnect — delete
is idempotent, so a connection that never logged in is a harmless no-op.
Inside regular reducers, `players::resolve_caller(ctx)` resolves
`ctx.sender` to `player_id` via the session table — the single chokepoint
for identity-to-player resolution.

The function relies on an invariant: any `PlayerSession.player_id` must
reference an existing `Player` row. Maintained by routing every `Player`
deletion through `players::delete_player(ctx, player_id)`, which cascades
session cleanup, marks every `Card` whose `macro_zone` or `owner_id`
references the player dead (via `mark_card_dead`, deduped), and only
then removes the `Player` row.

---

## World tiles — the `zones` table

World tiles are dense: an 8×8 chunk has 64 cells, and within a chunk
most cells share `(card_type, card_category)`. A per-tile `Card` row
would be 64× the bookkeeping for one chunk. Instead, a [`Zone`](src/zones.rs)
row stores a whole chunk:

| Column | Notes |
| --- | --- |
| `zone_id` | Auto-inc PK. Logical identity is `(layer, macro_zone)` — `zone_id` exists only because tables need a single-field PK. |
| `layer` | Indexed. Match `Card.layer` for any world card in this chunk. |
| `macro_zone` | Indexed. Packed `(zone_q:i16, zone_r:i16)`. |
| `packed_definition` | `[card_type:u4][card_category:u4]` — shared by every cell. The high byte of every cell's full `u16 packed_definition`. |
| `t0..t7` | Eight `u64`s, byte-packed. Each `u64` holds one row of 8 cell `definition_id`s, low byte first. Row-major: `cell_index = local_r * 8 + local_q`. |
| `delta_t` | Lag at write time. |

`definition_id == 0` is the empty-cell sentinel. Helpers in
[`zones.rs`](src/zones.rs) — `LocalCoord`, `read_cell` / `write_cell`,
`cell_packed_definition(zone_byte, id)`, `find_zone(layer, macro_zone)`,
`insert_empty_zone`, `lookup_cell`, `set_cell`. There are no
client-callable reducers on this table; world content changes happen
through other reducers (recipe completions, `bootstrap`, future world
edit reducers) that call into the cell helpers.

When a world tile needs first-class card state (a tree being chopped, a
creature occupying a tile), the plan is to materialize a `Card` row at
that micro_zone and treat the zone cell as the default appearance until
the card goes away. Today nothing does this; the zone is the sole source
of truth for world tile identity.

---

## Procedural map generation — `mapgen.rs`

Pure and deterministic: the same `(zone_q, zone_r)` always produces the
same cell layout across runs. No `ReducerContext`, no RNG state, no I/O —
generation is hash-driven so the world is infinite-on-demand without
persisting a seed.

Per cell:

1. Sample a [`Climate`](src/mapgen.rs) vector — independent value-noise
   channels for `temperature` and `humidity`.
2. For each biome declared in [`data/biomes.json`](../data/biomes.json),
   compute an inverse-square weight from the cell's climate point to
   the biome's center.
3. Sum each biome's tile distribution scaled by that biome's weight.
4. Weighted-pick a `definition_id` from the combined distribution using
   a separate hash channel.

Tile keys are resolved through `find_packed("tile/default/<key>")` at
biome-registry build time so a typo in `biomes.json` fails loudly once,
not at every fill. `mapgen::fill_zone_cells(zone_q, zone_r, &mut rows)`
is the entry point used by the `bootstrap` reducer.

---

## Authority model

The **server** is authoritative for:

- Card identity (`card_id`, `packed_definition`, `data`).
- Inventory membership (`macro_zone`).
- World card position and stack state (when `layer >= LAYER_WORLD`).
- Action lifecycle — start, complete, cancel.
- World tile content (`zones` rows).

The **client** is authoritative for:

- Inventory layout — stacking, ordering, pixel positions of inventory
  cards. Persisted client-side. The server zeros `micro_zone` /
  `micro_location` for inventory inserts and only honors a server-supplied
  position when `(micro_zone & MICRO_ZONE_LOCAL_Q_MASK) != 0` ("trust this
  placement"); `insert_card_row` deliberately doesn't set that bit, so
  client-side fiddling stays free.

---

## Client → server protocol

### Registration / login

Before calling any other reducer, a fresh connection must establish a
session by calling:

```rust
claim_or_login(name: String)
```

If no `Player` with that normalized name exists, one is created. Either way,
a `PlayerSession` is created (or replaced) linking the caller's current
`Identity` to that `player_id`. After that, `resolve_caller` succeeds and
other reducers can run.

> **Trust-on-first-use.** `claim_or_login` performs no authentication —
> anyone calling it with a given name becomes that player. This is
> intentional for the POC and **must** be replaced with token-based or
> external auth before the module is exposed to untrusted clients.

### Stack submission

```rust
submit_inventory_stacks(stacks: Vec<InventoryStack>)

struct InventoryStack {
    root: u32,
    layer: u8,
    macro_zone: u32,
    micro_zone: u8,
    micro_location: u32,
    stack_up: Vec<u32>,    // up to MAX_STACK_BRANCH (16)
    stack_down: Vec<u32>,  // up to MAX_STACK_BRANCH (16)
}
```

The submission carries the **root's** full row state (`layer`,
`macro_zone`, `micro_zone`, `micro_location`); chain children inherit
the chain's `(layer, macro_zone)` and have their own `micro_zone` /
`micro_location` written by the server's `mirror_stack` to point at
their parent.

Mixed-layer chains are legal — the server's `mirror_stack` migrates
every member to the chain's effective location. This supports
inventory↔world transitions in either direction with no extra API
surface: drag a card from inventory onto a world hex to migrate it up;
drag it off back into a chain rooted on an inventory card to migrate
it down.

The client decides which stacks to include. The rule is: any stack the
player is committing or whose composition has changed since the last
submission. The **client pre-filter** in `pixijs/src/actions/ActionManager.ts`
runs the same priority/upgrade matcher the server runs (per
[recipe-upgrade.md](../docs/recipe-upgrade.md)) and skips submissions
that wouldn't change anything.

### Server validation per submission

Whole-submission bound:

- `stacks.len() <= MAX_STACKS_PER_SUBMISSION` (256). Above this, reject before
  any per-stack work.

Then for each `InventoryStack`, in order:

1. **Bounds**: `stack_up.len() <= 16`, `stack_down.len() <= 16`.
2. **Single-stack-per-card**: no card_id appears in more than one submitted
   stack within the same call.
3. **Layer + caller authority**: every chain card's current row resolves to
   either `layer == LAYER_INVENTORY` with `macro_zone == caller's
   player_id`, or `layer >= LAYER_WORLD` (world cards are interactable
   by anyone — future work will add zone-proximity / permission rules).
4. **Target authority**: the stack's *target* `(layer, macro_zone)`
   must also be authorized — inventory targets must be the caller's,
   world targets are open. Failures route the chain to the caller's
   inventory at a known-good loose state via `return_chain_to_inventory`
   rather than aborting the whole submission.

After validation, `mirror_stack` writes the chain into row state.
`actions::process_top_branch` and `process_bottom_branch` then run the
upgrade machinery.

### Debug / dev reducers

- `debug_spawn(player_id, card_key)` — one-off card spawn into a
  player's inventory. Resolves `card_key` against `cards/id.json`.
- `debug_spawn_world(player_id, card_key, world_q, world_r)` — spawn a
  world card at hex `(world_q, world_r)`. Derives zone + local coords
  from the world coords and writes an authoritative placement.
- `bootstrap()` — seeds four world zones around origin via
  `mapgen::fill_zone_cells`, plus the cards listed in
  `data/bootstrap/bootstrap.json` to player_id 1's inventory. Idempotent
  on zones (replace-on-insert), additive on cards.

These bypass authentication but still go through `insert_card_row` /
`insert_card_row_at_position` so the chokepoint validations apply.

---

## Action machinery

Earlier iterations exposed `start_action` / `delete_action` as public
reducers. That was a security hole: any connected client could call
them with arbitrary arguments. The current design keeps action
lifecycle purely **implicit** — driven by validated stack submissions
and card creations. The only way a client influences action state is
via the reducers it's already authenticated against:
`submit_inventory_stacks` and the `insert_card_row` chokepoint
(through whatever reducer creates a card).

**Tables involved:**

- [`Action`](src/actions.rs) (public) — one row per in-progress action.
- [`MagneticAction`](src/magnetic.rs) (public) — one row per outer
  magnetic recipe in its slot-fill phase.
- `card_holds` (private) — claims a card to an action. PK on `card_id`
  enforces "one action per card." Walked by `action_id` btree on cancel.
- `action_scheduler` (private, scheduled) — drives `complete_action`
  when an action's duration elapses.
- `magnetic_inputs` (private, scheduled) — drives `magnetic_tick` for
  the slot-fill loop.

**Trigger model:** "caller passes the stack" — actions never walk the
cards table to reconstruct chains, and there is deliberately no
client-callable `start_action` / `delete_action` reducer.
Recipe-completion-triggers-another-recipe falls out automatically
because every product card created during completion is inserted via
`insert_card_row`, which runs the on_create matcher against it.

**Upgrade machinery:** `process_top_branch` / `process_bottom_branch`
walk every potential actor along the submitted chain, build a *visible
window* (`build_visible_chain` — actor outward, including cards free
or claimed by the actor's own action, stopping at any other action's
claim), score all recipes with `score_recipe_for_actor`, and apply
the four-way upgrade decision in `process_actor_candidate`:

| Current action | Best recipe at actor | Outcome |
| --- | --- | --- |
| none | none | nothing |
| none | r | start r |
| a | none | cancel a |
| a | same recipe AND slot fillers unchanged | keep a running |
| a | different recipe, or fillers moved | cancel a, start the winner |

`complete_action` runs `recipe_still_satisfies_claim` as defense-in-depth
before producing or consuming. The matcher uses an `entity_match_weight`
scorer (`Card`=4, `Aspect`=3, `Type`=2, `Any`=1; `And` sums children;
`Or`/`WeightedOr` take the satisfying branch) and a lex-ordered
`MatchWeight { tile_weight, root_weight, slot_weight }` to pick the
highest-weight match across recipes.

The same priority evaluation runs on the client as a pre-filter; both
sides read the same recipe JSON but the evaluation logic must be kept
in lockstep manually. See
[`data/recipes/AGENT.md`](../data/recipes/AGENT.md) ("Where this is
implemented") and [`docs/recipe-upgrade.md`](../docs/recipe-upgrade.md).

**Death cascade:** `delete_action_rows` is the single chokepoint for
action removal. It releases magnetic state via `magnetic::release`,
clears `card_holds` for the action, deletes the `action_scheduler`
row, and calls `mark_action_dead` (which sets `FLAG_ACTION_DEAD` plus
`FLAG_ACTION_CANCELED` or `FLAG_ACTION_COMPLETE`, stamps `delta_t`,
and queues the actual delete via `pending_action_deletions`).

---

## Magnetic action machinery

Magnetic recipes carry a `magnetic: "top" | "bottom"` field that flips
them into "server pulls inputs from the player's inventory" mode.
Lifecycle:

1. **Match an outer magnetic recipe** → install via `magnetic::install`,
   which inserts a `MagneticAction` row (no `Action` row yet) and
   schedules a `magnetic_inputs` tick at `recipe.interval`.
2. **Tick** → walk what's already attached to the actor, stamp
   `position_hold` + `drop_hold` and a `CardHold` on each, then top up
   from the player's inventory by walking the `owner_id` btree for the
   first non-blocked card that satisfies the next slot's `Entity`.
   Placement writes `local_q = 1` into `micro_zone`'s high 3 bits so
   the client trusts the position, and promotes the placed card's
   `layer` to the actor's layer (so a card scooped from inventory onto
   a world tile lands on the world layer alongside the rest of the chain).
3. **Inner queued** → when an inner recipe's slot list is fully filled,
   queue that inner as a normal `Action` (with
   `Action.flags & FLAG_ACTION_MAGNETIC` set and a 4-bit sub-id pointing
   back at which inner filled), fire the **outer** recipe's
   `output_success` / `reagents`, remove the magnetic_action via
   `mark_magnetic_action_dead(complete=true)`.
4. **Timeout** → outer's `duration` is the magnetic-phase loop cap in
   *ticks*. When `loop_count > cap`, fire the outer's `output_failure`,
   release `position_hold` + `drop_hold` on every placed card, remove
   the magnetic_action via `mark_magnetic_action_dead(complete=true)`
   (still complete, just with a failure outcome).
5. **Cancel** (e.g. anchor removed) → release flags, remove via
   `mark_magnetic_action_dead(canceled=true)`. No outputs fire —
   cancellation is distinct from completion.

Stack-state constants (`STACK_STATE_LOOSE`, `STACK_STATE_TOP`,
`STACK_STATE_BOTTOM`, `STACK_STATE_HEX_ROOT`) live in
[`magnetic.rs`](src/magnetic.rs) and **must be kept in lockstep with
the client's `cardData.ts`**.

---

## Scheduled-reducer lag — `delta_t`

Every public-table row carries a `delta_t: u8` column. Each unit is
**32 ms**. The value is the gap between when a scheduled reducer was
*supposed* to fire and when it *actually* ran — so the client can
back-date its animations by `32 * delta_t` ms instead of treating the
row update as "happening now."

**Default `0`** for client-driven writes (the call stack is outside
any scheduled reducer). Scheduled reducers (`complete_action`,
`magnetic_tick`, `reap_dead_card`, `reap_dead_action`,
`reap_dead_magnetic_action`) install a `delta_t::Guard` at their entry
point via `delta_t::enter(delta_t::compute(scheduled_micros, now_micros))`;
every public-table write inside that scope reads the guard's value
via `delta_t::current()` and stamps the field. The guard restores the
previous value on drop.

Saturating semantics: lag larger than `u8::MAX * 32 ms` (~8.16 s)
clamps. A clamp event means the client-side compensation buffer was
overrun anyway; logging it is the client's job.

The client uses this to **subtract from a 1 second display buffer**
(`pixijs/src/state/DataManager.ts::deltaTMsFromRow`): rows arrive
delayed in the client map by `delayMs - 32 * deltaT` ms, so server
lateness consumes the buffer rather than stacking on top of it.

---

## File map

- [src/lib.rs](src/lib.rs) — module root; declares `actions`, `cards`, `debug`, `definitions`, `delta_t`, `magnetic`, `mapgen`, `packing`, `players`, `zones`.
- [src/packing.rs](src/packing.rs) — `pack_definition` / `unpack_definition` (`Card.packed_definition`), `pack_recipe` / `unpack_recipe` (`Action.recipe`), `pack_world_macro_zone` / `unpack_world_macro_zone` (world `Card.macro_zone` and `Zone.macro_zone`), `pack_world_micro_location` / `unpack_world_micro_location` (loose-world-card pixel coords).
- [src/cards.rs](src/cards.rs) — `Card` table; `LAYER_INVENTORY` / `LAYER_WORLD` constants; `MICRO_ZONE_LOCAL_Q_MASK` / `MICRO_ZONE_LOCAL_Q_ONE` (the trust-this-position signal); `FLAG_CARD_POSITION_HOLD` / `FLAG_CARD_DROP_HOLD` / `FLAG_CARD_DEAD` flag bits; the dead-card reaper (`mark_card_dead`, `pending_card_deletions`, `reap_dead_card`); `clear_action_hold_flags` / `clear_hold_flags_on_inventory_landing` helpers; the `insert_card_row` chokepoint (zeros position fields, runs the on_create matcher) plus the `insert_card_row_at_position` opt-in; `InventoryStack` (with full root-row state); the `submit_inventory_stacks` reducer + `mirror_stack` / `update_chain_member` / `return_chain_to_inventory`; `MAX_STACK_BRANCH` / `MAX_STACKS_PER_SUBMISSION` bounds.
- [src/players.rs](src/players.rs) — `Player` and `PlayerSession` tables; `resolve_caller(ctx)`; `validate_player_name` / `normalize_player_name` / `MAX_PLAYER_NAME_LEN`; the `delete_player` cascade helper (calls `mark_card_dead` for every owned/held card); the `claim_or_login` reducer (trust-on-first-use); the `client_disconnected` lifecycle reducer.
- [src/actions.rs](src/actions.rs) — `Action`, `ActionScheduler`, `CardHold`, `PendingActionDeletion` tables; `complete_action` and `reap_dead_action` scheduled reducers (the only reducers in the module — both guarded against client-spoofed early invocation); `process_top_branch` / `process_bottom_branch` / `try_start_on_create_action` invoked from `cards.rs`; `pack_participants` / `unpack_participants`; `FLAG_ACTION_DEAD` / `FLAG_ACTION_CANCELED` / `FLAG_ACTION_COMPLETE` flag bits and `mark_action_dead` / `delete_action_rows` chokepoints; the matcher (`score_recipe_for_actor`, `entity_match_weight`, `MatchWeight`, `recipe_still_satisfies_claim`).
- [src/magnetic.rs](src/magnetic.rs) — `MagneticAction` table, `magnetic_inputs` (private, scheduled) and `pending_magnetic_action_deletions`; `magnetic_tick` and `reap_dead_magnetic_action` scheduled reducers; `install` / `release` / `cancel` / `mark_magnetic_action_dead` lifecycle helpers; the slot-fill loop that pulls cards from the player's inventory each tick; stack-state constants (`STACK_STATE_LOOSE` / `_TOP` / `_BOTTOM` / `_HEX_ROOT`); `FLAG_ACTION_MAGNETIC` and the `pack_action_flags` / `unpack_action_subid` helpers; `MAGNETIC_MAX_SLOTS = 5`.
- [src/definitions.rs](src/definitions.rs) — `CardDefinition`, `Aspect`, `RecipeDef` registries built lazily from `data/card_types.json`, `data/aspects.json`, `data/cards/*.json` (+ `cards/id.json`), and `data/recipes/*.json` (+ `recipes/id.json`). Card lookup: `decode_definition(packed)`, `find_packed(path)`, `find_packed_by_key(key)`. Aspect side: `aspect_id`, `aspect`, `aspects`. Recipe side: `recipe(index)`, `find_recipe(id)`, `recipes_of_type(rt)`. The `Entity` enum carries the leaf forms (`Card`, `Aspect`, `Type`, `Any`) plus `And` / `Or` / `WeightedOr`. `RecipeDef.tile` (forward-looking world precondition, scored 0 today), `RecipeDef.hex` (re-resolves on every upgrade pass), `RecipeDef.magnetic` (top / bottom). `is_hex_type(type_id)` returns whether a card_type's shape is `"hex"`. All registries stored as `OnceLock<Result<…, String>>` so a build failure is recorded once and returned to every subsequent lookup.
- [src/zones.rs](src/zones.rs) — `Zone` table; `LocalCoord` (zone-local `(q, r)` with `from_micro_zone` / `to_micro_zone`); `read_cell` / `write_cell` (raw bit math); `lookup_cell` / `set_cell` (table-level CRUD); `find_zone(layer, macro_zone)`; `insert_empty_zone`. Pure data — no reducers.
- [src/mapgen.rs](src/mapgen.rs) — procedural map generation. `Climate` sampling, biome registry from `data/biomes.json`, `fill_zone_cells(zone_q, zone_r, &mut rows)`. Hash-driven and deterministic — same coordinates always produce the same cells.
- [src/delta_t.rs](src/delta_t.rs) — scheduled-reducer lag tracking. `current()` / `enter(value)` / `compute(scheduled_micros, now_micros)`. Thread-local guarded so nested scheduled fires round-trip correctly.
- [src/debug.rs](src/debug.rs) — development-only reducers and bootstrap. Public surface: `spawn_card_for_player(ctx, player_id, card_key) -> Card`, `debug_spawn(player_id, card_key)`, `debug_spawn_world(player_id, card_key, world_q, world_r)`, `bootstrap()`. Bootstrap creates four world zones around origin via `mapgen::fill_zone_cells`, then appends `data/bootstrap/bootstrap.json`'s card list to player_id 1's inventory. Idempotent on zones, additive on cards.

---

## Not yet implemented

- **Real authentication** — `claim_or_login` is trust-on-first-use, no
  password / token / external auth. Anyone can become any player by name.
  Replace before opening the module to untrusted clients.
- **Player deletion** — `delete_player` exists as the cascade-cleanup
  helper, but nothing calls it. Add an admin reducer (or trigger) when
  player deletion becomes a real flow.
- **Multi-slot magnetic inners + non-hex anchors** — today the magnetic
  system implements only the despair-recipe path (hex-anchored outer
  with a single inner that has only a `root`). Multi-slot inners and
  non-hex anchors will reject at install / tick rather than misbehave.
- **Tile context for recipes** — `RecipeDef.tile` is parsed and threads
  through `MatchWeight`, but the matcher always scores it `0` until the
  world-tile-context lookup is wired up.
- **World permission / proximity rules** — any caller can submit a stack
  involving any world card today. The TODO is to gate world-card
  submissions on the caller's soul being within some hex-distance of
  the cards.
