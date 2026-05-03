# Resonant Dust — SpacetimeDB Module

This is the authoritative server module. It owns the canonical state of every
card and player. Clients subscribe to the public tables here to render
inventories; clients call reducers here to request state changes.

For language/SDK rules see [../AGENTS.md](../AGENTS.md). This file documents
the **game model** encoded by the tables and reducers in `src/`.

The current proof of concept is **inventory-only** — there is no world board,
no stack-state tracking on the server, no actions, no ownership tracking. The
table schema reflects only what's used today; world / action / ownership
fields will land alongside the code that needs them.

---

## Card storage model

A card is a single row in the `cards` table:

| Column              | Type | Meaning                                                                  |
| ------------------- | ---- | ------------------------------------------------------------------------ |
| `card_id`           | u32  | Primary key. Auto-increment. Unique across all cards.                    |
| `layer`             | u8   | Where the card lives. Currently always `LAYER_INVENTORY` (1); world layers will be added later. |
| `macro_zone`        | u32  | Inventory holder's `player_id`. Subscription discriminator. Indexed.     |
| `micro_zone`        | u8   | World cards: `[local_q:u3][local_r:u3][stack_state:u2]` — in-zone hex coords plus the card's stack role (bits 1..0). Inventory cards: held at 0. |
| `micro_location`    | u32  | World cards: variant per `micro_zone`'s `stack_state` — parent `card_id` or packed `(i16 x, i16 y)` pixel coords. Inventory cards: held at 0. |
| `owner_id`          | u32  | Player who owns this card. Not necessarily the player whose inventory the card sits in. Indexed. |
| `packed_definition` | u16  | `[card_type:u4][card_category:u4][definition_id:u8]`. Card identity for the renderer/rules. |

Inventory layout (stacking, ordering, pixel positions) is **client-side
state** — the server does not store any of that. The client persists it
(cookies / localStorage / IndexedDB).

---

## Subscription model — why `macro_zone` is a `player_id`

Clients receive data by subscribing to public tables. They scope those
subscriptions by `macro_zone`. Today every card lives in some player's
inventory, so `macro_zone` is set to that player's `player_id` and the client
that owns the inventory subscribes on `macro_zone == own_player_id`.

The field is called `macro_zone` rather than `inventory_player_id` because
when world cards land, the same field will also hold packed
`(zone_q:i16, zone_r:i16)` axial coordinates.

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
| `name_normalized` | String | Unique. Lowercased mirror of `name` for case-insensitive uniqueness. Always equal to `normalize_player_name(&name)`. |
| `layer`           | u8     | Layer the player's soul currently occupies (world layer).                                               |
| `macro_zone`      | u32    | World macro_zone the soul currently occupies. Indexed.                                                  |
| `micro_zone`      | u8     | In-zone position of the soul: `[local_q:u3][local_r:u3][stack_state:u2]`. `0` while unplaced.            |
| `micro_location`  | u32    | Within-`micro_zone` position of the soul. Variant per stack state — parent `card_id` or packed `(i16 x, i16 y)` pixel coords. |

The world fields (`layer`, `macro_zone`, `micro_zone`) describe where the
player's soul-card sits in the world. They're meaningful only once the world
board lands; until then they exist on the row but are inert.

`validate_player_name` rejects empty names, whitespace-only names, names
containing control characters, and names exceeding `MAX_PLAYER_NAME_LEN`
bytes. Registration must call it before inserting.

`player_sessions` (private) — the bridge between an active connection's
`Identity` and the player's `player_id`:

| Column      | Type     | Notes                                                                  |
| ----------- | -------- | ---------------------------------------------------------------------- |
| `identity`  | Identity | Primary key. The caller's current connection identity.                 |
| `player_id` | u32      | The stable player this session belongs to. Indexed for cleanup queries. |

A login reducer (not yet written) creates the `PlayerSession` row once the
caller authenticates against `Player.name_normalized`. The
`client_disconnected` lifecycle reducer in [src/players.rs](src/players.rs)
removes the row on disconnect — delete is idempotent, so a connection that
never logged in is a harmless no-op. Inside regular reducers,
`players::resolve_caller(ctx)` resolves `ctx.sender` to `player_id` via the
session table — the single chokepoint for identity-to-player resolution.

The function relies on an invariant: any `PlayerSession.player_id` must
reference an existing `Player` row. Maintained by routing every `Player`
deletion through `players::delete_player(ctx, player_id)`, which cascades
session cleanup, deletes every `Card` whose `macro_zone` or `owner_id`
references the player (deduped), and only then removes the `Player` row.
A returned `player_id` is therefore trusted by callers without a follow-up
`players()` lookup.

---

## Authority model

The **server** is authoritative for:

- Card identity (`card_id`, `packed_definition`, `data`).
- Inventory membership (`macro_zone`).

The **client** is authoritative for:

- Inventory layout — stacking, ordering, pixel positions. Persisted client-side.

When the world board lands, the server will become authoritative for world
positions and actions. Client authority over inventory layout will remain.

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

### Inventory stack submission

The client calls `submit_inventory_stacks` whenever it does something that
should affect server state. Today that's just membership validation; once
actions exist this is where they'll be cancelled and triggered.

```rust
submit_inventory_stacks(stacks: Vec<InventoryStack>)

struct InventoryStack {
    root: u32,
    stack_up: Vec<u32>,   // up to MAX_STACK_BRANCH (16)
    stack_down: Vec<u32>, // up to MAX_STACK_BRANCH (16)
}
```

The client decides which stacks to include. Once actions exist, the rule will
be: any stack the player is actively committing, plus any stack that lost
cards from a stack with an in-progress action. Today, with no actions,
calling this reducer is just membership validation.

### Server validation per submission

Whole-submission bound:

- `stacks.len() <= MAX_STACKS_PER_SUBMISSION` (256). Above this, reject before
  any per-stack work.

Then for each `InventoryStack`, in order:

1. **Bounds**: `stack_up.len() <= 16`, `stack_down.len() <= 16`.
2. **Single-stack-per-card**: no card_id appears in more than one submitted
   stack within the same call.
3. **Layer**: every card_id resolves to a card row with `layer == LAYER_INVENTORY`.
4. **Membership**: every card_id resolves to a card row with
   `macro_zone == caller's player_id`.

Failure on any of these aborts the whole transaction.

### Not yet implemented

- **Actions**: cancel-check, trigger-check, the Action table itself, and
  recipe definitions.
- **World layers**: no world board, no world cards.
- **Ownership**: no `owner_id` field. Cards are tied to inventories only.
- **Position-field semantics for world cards**: `micro_zone` (now carrying
  `stack_state` in its low 2 bits) and `micro_location` are present on the
  table but only meaningful once world layers exist.
- **Real authentication**: `claim_or_login` is trust-on-first-use, no
  password / token / external auth. Anyone can become any player by name.
  Replace before opening the module to untrusted clients.
- **Player deletion**: `delete_player` exists as the cascade-cleanup
  helper, but nothing calls it. Add an admin reducer (or trigger) when
  player deletion becomes a real flow.

---

## File map

- [src/lib.rs](src/lib.rs) — module root; declares `cards`, `packing`, `players`, and (behind the `debug` cargo feature) `debug`.
- [src/packing.rs](src/packing.rs) — `pack_definition` / `unpack_definition` for `packed_definition`. The other packing helpers came out with their fields and will come back with the world board.
- [src/cards.rs](src/cards.rs) — `cards` table, the `insert_card_row` chokepoint helper (validates layer + target-player existence), the `InventoryStack` parameter type, the `submit_inventory_stacks` reducer, and `MAX_STACK_BRANCH` / `MAX_STACKS_PER_SUBMISSION` bounds.
- [src/players.rs](src/players.rs) — `players` and `player_sessions` tables, the `resolve_caller(ctx)` helper, `validate_player_name` / `normalize_player_name` / `MAX_PLAYER_NAME_LEN`, the `delete_player` cascade helper, the `claim_or_login` reducer (trust-on-first-use), and the `client_disconnected` lifecycle reducer.
- [src/actions.rs](src/actions.rs) — `actions`, `action_scheduler` (scheduled), and `card_holds` tables; the `complete_action` scheduled reducer (the only reducer in the module, and guarded against client-spoofed early invocation by a scheduler-row lookup and an end-time check); the helper functions `process_top_branch`, `process_bottom_branch`, `try_start_on_create_action` invoked from `submit_inventory_stacks` and `insert_card_row` in `cards.rs`; and `pack_participants` / `unpack_participants` (with `MAX_PARTICIPANT_LENGTH = 0xF`) for the `Action.participants` u8 (`[up_length:u4][down_length:u4]`, actor implied by `card_id`). **What `CardHold` claims**: actor + slot fillers only — the chain root is **not** held even when `recipe.root` is set. Holding it would make the root a contention point, e.g. blocking `[attack, sword] + human` and `[heal, anima] + human` from running concurrently. The chain root is also not stored on the `Action` row; `recipe.root` is just a pre-condition the matcher re-checks against `branch_chain[0]` on every upgrade pass, so a drifted-but-still-matching root keeps the action running and a no-longer-matching root cancels it. Reagent index `0` for stack recipes is therefore a no-op today (the chain root isn't recoverable at completion time); for `OnCreate`, actor == root so reagent `0` consumes the actor card. **Upgrade machinery**: `process_*_branch` walks every potential actor along the submitted chain, builds a *visible window* (`build_visible_chain` — actor outward, including cards free or claimed by the actor's own action, stopping at any other action's claim), scores all recipes with `score_recipe_for_actor`, and applies a four-way upgrade decision in `process_actor_candidate`: no current and no match → nothing; current and no match → cancel; no current and match → start; current and match → keep running iff same recipe AND `slot_fillers_unchanged` (strict set equality on the claim — no root to subtract). `complete_action` runs `recipe_still_satisfies_claim` as defense-in-depth before producing or consuming. The matcher uses an `entity_match_weight` scorer (per-leaf weights `Card`=4, `Aspect`=3, `Type`=2, `Any`=1; `And` sums children; `Or`/`WeightedOr` take the satisfying branch) and a lex-ordered `MatchWeight { tile_weight, root_weight, slot_weight }` to pick the highest-weight match across recipes — strict tier ordering means a satisfied `tile` outscores any combination of `root` and slot weights, etc. **The same priority evaluation also runs on the client as a pre-filter**: the client decides whether a commit would actually change server-side state before sending it. The server is the authoritative evaluator and re-runs the calculation independently — it doesn't trust the client's prediction. Both sides read the same recipe JSON, but the evaluation logic has to be kept in lockstep manually; see `data/recipes/AGENT.md` ("Where this is implemented" and the upgrade-rule subsection) for the recipe-author view and the synchronization requirement, and [recipe-upgrade.md](../../docs/recipe-upgrade.md) for the upgrade-rule mechanics in full. Trigger model: "caller passes the stack" — actions never walk the cards table to reconstruct chains, and there is deliberately no client-callable `start_action` or `delete_action` reducer (anything a client can do to action state happens implicitly through validated stack submissions and card creations). Recipe-completion-triggers-another-recipe falls out automatically because every product card created during completion is inserted via `insert_card_row`, which runs the on_create matcher against it.
- [src/definitions.rs](src/definitions.rs) — `CardDefinition`, `Aspect`, and `RecipeDef` registries built lazily from `data/card_types.json`, `data/aspects.json`, `data/cards/*.json` (+ `cards/id.json` for stable `definition_id`s), and `data/recipes/*.json` (+ `recipes/id.json` for stable recipe IDs). Card lookup: `decode_definition(packed)`, `find_packed(path)`, `find_packed_by_key(key)` (O(log n) via the `by_key` map populated from `cards/id.json`). Aspect side: `aspect_id`, `aspect`, `aspects`. Recipe side: `recipe(index)`, `find_recipe(id)`, `recipes_of_type(rt)`. The `Entity` enum carries five leaf forms — `Card(String)`, `Aspect(AspectId, i32)`, `Type(u8)` (resolved from `"@<type_name>"` at parse time), `Any` (wildcard from `"any"`), plus the `And` / `Or` / `WeightedOr` composites. `RecipeDef` carries an optional `tile: Option<Entity>` field for forward-looking world-layer tile preconditions (parsed today, scored 0 by the matcher until tile context is wired up). All lookups return `Result<…, String>`. Each registry is stored as `OnceLock<Result<Registry, String>>`, so a build failure (malformed JSON, unknown aspect, unknown card type, missing entry in `id.json`, invalid hex color, duplicate aspect on a card, unknown recipe type, malformed entity grammar, missing duration fallback, …) is recorded once and returned to every subsequent lookup rather than re-panicking.
- [src/debug.rs](src/debug.rs) — development-only reducers (currently `debug_spawn`). Compiled only when `--features debug` is set. Bypasses authentication; should not ship to production.
