# AGENTS.md — `shard` module (the unified data shard)

## Purpose
SpacetimeDB 2.1.0 server module (Rust → wasm32). The **unified data-shard
module** — `resonantdust_shard`, formerly the separate `cards` + `regions`
modules. It holds the authoritative *packed* state and arbitrates holds; gameplay
validation/planning lives in the **gate**, which drives this module through a
narrow write surface ([`src/gate_api.rs`](src/gate_api.rs)). Compiles and runs
inside a Docker-hosted SpacetimeDB instance — never against a host `spacetime`
CLI.

Players / login live in the separate [`players`](../players/AGENTS.md) auth
module; chat in [`chat`](../chat/AGENTS.md). Nothing here references them across
databases (SpacetimeDB modules can't write cross-DB — the gate bridges).

## One binary, two databases
The same wasm is published to BOTH database families, and a card's `card_id`
encodes which one it belongs to (top bit = `card_db`, via
[`shared/codec/src/packed.rs`](../../../../shared/codec/src/packed.rs) `pack_card_id` /
`card_db_of`) so the gate routes reads/writes with no lookup:

- **cards DB** (`resonantdust-<env>-cards-N`) — owner cards + souls (`cards`,
  `souls`). The hot card/soul state for the players assigned to it.
- **region DB** (`resonantdust-<env>-regions-N`) — world terrain + tile-cards
  (`zones`, `regions`, `card_shards`, and tile-cards in the same `cards` table).

A single binary stamps the right `card_db` bit via the private one-row
[`ShardIdentity`](src/cards.rs) table (`{card_db, shard}`; `next_card_id` reads
it). Unseeded it defaults to `(CARD_DB_CARDS, DATA_SHARD)`, so a cards DB needs no
seeding; a region DB is seeded once after publish by `set_shard_identity` (the
gate / `bin/st` does this). **Region-only tables are simply empty on a cards DB
and vice-versa** — the GC sweep runs the union of both and no-ops where empty.

## Build & iterate
- **Always** use `bin/st build shard` (or `bin/st build` for every module). It
  runs `cargo build --release --target wasm32-unknown-unknown` in the build
  container (workdir `/workspace/server/modules/shard`), then regenerates the
  gateway bindings into `gateway/src/bindings/shard/`. (The old
  `content/gen-ids.py` step is gone — def ids are content-derived in the loader,
  not pre-generated. The client is Rust now: it links `codec` directly, so there
  are no TS shard bindings to regenerate.) Don't reach for `cargo`, host
  `spacetime`, or ad-hoc `docker compose run`. `rustfmt` is absent in the image —
  the "could not format" warning is cosmetic, files are still written.
- **No DB is named `shard`.** `bin/st re cards` and `bin/st re regions` both
  publish *this* module to their respective DBs (see `bin/st` `DB_SOURCE`); the
  regions one also seeds `ShardIdentity`. `bin/redeploy` fans one `shard` source
  change out to both. `bin/st sql cards ...` / `bin/st select regions zones ...`
  for live queries (the table→DB map is in `bin/st`).
- TS bindings are generated; private tables (no `public`) are skipped during
  codegen: `card_id_counter`, `shard_identity`, `sequence_counter`,
  `pending_actions`, `gc_schedule`.

## Module map
| File | Role |
| --- | --- |
| [src/cards.rs](src/cards.rs) | The `cards` public table + the **canonical bitemporal write primitives**: `latest` / `prior_at` / `delete_at`, `create(_at)` / `update_with(_at)` / `write_at`. **`write_at` is the single write entry point** — every write fires `souls::on_card_write`, forward-propagates `flags_state` (`propagate_flag_diff_forward`), and cascades to deferred followers (`cascade_to_state_3_followers`). The `acquire_<name>` / `release_<name>` / `propagate_<name>_forward` refcount triplets (`position_hold` / `slot_share` / `drop_hold` / `slot_hold` / `touch` / `server`) via `decl_count_ctx!`. `next_card_id` (private `card_id_counter`) + the private `ShardIdentity` table and `identity` / `set_identity`. Owner-walk helpers `owning_player` / `owning_soul`. **Owns the time-discipline contract:** `TIME_DRIFT_BUFFER_MS` / `BACKWARD_GRACE_MS`, `WORLD_PLAYER_ID`, `now_ms`, `effective_now_ms`. |
| [src/tiles.rs](src/tiles.rs) | Tile-card helpers over the canonical `cards` primitives: `find_or_create_tile_card` (promote a zone tile → `card_type = 7` card), `find_tile_card_at`, `set_tile_stock`, `acquire_tile_hold` / `release_tile_hold` (the exclusive-cut CAS is in `acquire_tile_hold`), `tile_full_view`. Replaced the old `regions` module's drifted partial copy of the write primitives — tile-cards now get forward-prop for free. |
| [src/zones.rs](src/zones.rs) | `zones` public table (history-style). `latest_for(macro_zone)`, `next_zone_id`. Tile slots `[def_id:u12 \| stock0:u2 \| stock1:u2]` in `t0..t15`. Setters `set_tile_at`, `set_tile_stock_at`, and `fold_tiles_at` (batched per-zone fold used by GC demotion, with per-cell forward-prop). |
| [src/regions.rs](src/regions.rs) | `regions` table + the spawn-gating reducers `ensure_region` (client-driven region declaration) and `request_zone` (region-gated on-demand zone spawn). **`regions` is CURRENT-VALUE** — one mutable row per `macro_region` keyed by `macro_region` (NOT version-history; no `valid_at`). `zone_available` is a plain monotonic bit-accumulator: `set_available_bit` `\|=` on zone create, `clear_available_bit` `&=` on removal; clear `zone_presence` to stop regen. **Worldgen moved to the gate** — it supplies the tile bytes; this module just gates and stores. |
| [src/card_shards.rs](src/card_shards.rs) | `card_shards` public versioned subscription index — per-`data_shard` refcount of cards in this region shard, so a client subscribing to a region DB learns which cards shards to also subscribe to. Gateway-maintained (`acquire_card_shard` / `release_card_shard`), derived from the validated recipe. |
| [src/souls.rs](src/souls.rs) | `souls` public follower table (one row per soul card; mirrors position + packs corpus/anima/sollertia/aether counters into `stats`/`fatigued`/`injured`). **`on_card_write` is the single sync point** — `cards::write_at` calls it to mirror soul position. `apply_stat` is the gate-driven soul-stat delta path. |
| [src/place.rs](src/place.rs) | `place_card(client_time_ms, card_id, placement)` — unified place primitive (stack-onto-root in a direction, or loose-place at an address). Re-stamps descendants' `surface`/`macro_zone`; **never touches `owner_id`** (ownership ⊥ position). |
| [src/movement.rs](src/movement.rs) | `move_soul(client_time_ms, soul_id, path)` — client submits a precomputed path; server validates per-step adjacency/traversability/length and writes future-stamped soul rows. Tile lookups route through the tile-card-priority view. |
| [src/utilities.rs](src/utilities.rs) | `spawn_soul` (mint a player-soul card + its loadout) and `add_card(client_time_ms, soul_card_id, card_key)` (inventory spawn for the caller's soul). |
| [src/gate_api.rs](src/gate_api.rs) | **The gate's write surface — the coarse apply path.** `claim_pending` / `release_pending` (dedup), `apply_action` (cards DB) and `apply_action_tile` (region DB) — one coarse reducer per DB, see *Action pipeline*. `set_shard_identity` (deploy seed). Plus the shared `dispatch_hold` / `hold_field` helpers + the `stock_op` codes. |
| [src/gc.rs](src/gc.rs) | Single recurring `gc_schedule` row + `gc_sweep` (every 10 min, seeded by `init`, which also seeds the world origin region). Runs the **union** of sweeps: card retention (prior-version reap + dead-row policy), souls, tile-card **demotion** (`sweep_tile_card_demotions` — folds at-rest tile-cards back into the zone, back-dated; see below), zones/card_shards prior-version reaps. (`regions` is current-value — no version reap.) Each no-ops where its tables are empty. |
| [src/flags.rs](src/flags.rs) | Canonical flag field-routing: `state_flags()` / `bk_flags()` lazily load every `cards_state` / `cards_bk` mask+shift+max from the shared **`resonantdust_codec::flags`** registry (the runtime source — the old `flags.json` is gone) into `OnceLock`. **Read flag values through these, never hardcoded shifts.** |
| [src/pending_actions.rs](src/pending_actions.rs) | Private `pending_actions` dedup registry keyed on `hash(recipe_id, root, bindings)`. `install` / `is_in_flight` (via `claim_pending`); self-expires, orphan-reaped by GC. |
| [src/sequence.rs](src/sequence.rs) | Private `sequence_counter` + `next_sequence()` u16. **Load-bearing for `valid_at`** — every history write asks for a fresh sequence to disambiguate same-ms PKs. |
| [src/packed.rs](src/packed.rs) | Thin `pub use resonantdust_codec::packed::*;` re-export. Source of truth + tests live in the shared `codec` crate. |

## Action pipeline (gate-validated, coarse-applied)
There is **no `propose_action` reducer in this module** — the gate intercepts it,
validates + plans the recipe against the shared VM, and applies the result
through two coarse reducers, **one per database, each one transaction**:

- **`apply_action`** (cards DB) — for every bound card: acquire its masked hold
  **count** fields `@now_ms`, then release them + clear `pos_need`/`pos_want` +
  stamp `progress_style` `@completion_ms`; then the card-side effects (destroy /
  create / soul-stat). Holds arrive as a per-card **bitmask**
  (bit `i` = `hold_kind` `i`); the reducer still bumps the real refcount fields
  via the `acquire_*`/`release_*` helpers (forward-prop intact). A hold conflict
  returns `Err`, which **rolls the whole transaction back** (per-shard
  all-or-nothing).
- **`apply_action_tile`** (region DB) — promote the tile + acquire masked holds
  `@now_ms`, then release + read-modify-write stock `@completion_ms`. `slot_hold`
  is the concurrent-cut CAS.

The win over the retired decomposition (one reducer call per hold-kind / effect):
the client receives **one commit per shard** — a `now` row + a single
fully-formed `completion` row per card — instead of a half-written completion row
streaming across many commits (which it could promote mid-write → the cut-tree
flicker). The gate sequences tile-first (its exclusive `slot_hold` fails fast),
then cards; each coarse reducer writes its own self-expiring release rows, so a
partial cross-shard failure self-heals at `completion_ms`. Gate side:
[`gateway/src/apply.rs`](../../../../gateway/src/apply.rs).

There is no server-side action table — the action's state is the rows it stamped
plus the private `pending_actions` dedup row (orphan-reaped by GC).

## The `valid_at` pattern
`cards`, `zones`, `souls`, `card_shards` (and `players` in its module) share one
primary-key shape. (`regions` is the exception — it's current-value, keyed by
`macro_region`, no `valid_at`; see the `regions.rs` row above.)

```
valid_at: u64 = (time_ms: u48 << 16) | sequence: u16
```

- **`valid_at` is the primary key**; the logical id (`card_id` / `zone_id` / …) is
  a `#[index(btree)]` column. Rows for one id form a *history* ordered by
  `valid_at_time`.
- **Time is milliseconds.** `now_ms(ctx) = ctx.timestamp.to_micros_since_unix_epoch() / 1_000`.
- **The sequence gives PK uniqueness across same-ms writes.** `sequence::next_sequence`.
- **Same-(id, time_ms) writes are purged before insert** — `write_at` deletes any
  row at that exact `(id, time_ms)` first, so "last write at this (id, ms) wins"
  (needed for in-reducer accumulation, and for the coarse reducers' multiple
  in-transaction rewrites collapsing to one surviving row).
- **Future-stamped rows are first-class.** Completion effects are stamped at
  `valid_at_time > now`. `latest()` filters `valid_at_time ≤ now_ms`;
  **`prior_at(ctx, id, time_ms)`** is the "row current at the time we're writing"
  form every writer past `now` must use — never `latest()`.
- **Back-dating matters for the client clock.** The client promotes on a
  *buffered* `now`. GC tile-card **demotion** stamps the folded zone row at the
  demoted card's own (already-elapsed) `valid_at`, NOT `now_ms`, so the zone
  baseline is promotable the instant the card is reaped (else: the GC fold-back
  flash — tile snaps to the stale pre-card zone, then to the new baseline a
  buffer-length later).

Client mirror: the Rust client core's bitemporal stores (`client/core/src/world.rs`
`Cards` / `Zones`, current-at-`now` resolution).

## The `flags` columns
`Card` carries two `u32` flag hosts: **`flags_state`** (what is true about the
card; forward-propagated by `propagate_flag_diff_forward`) and **`flags_bk`**
(bookkeeping — refcount holds + dirty/preserve markers + tile-card stock + the
`micro_location` discriminator/stack fields; never bit-diff propagated). **Bit
positions are pinned by the `resonantdust_codec::flags` registry — that's
canonical (the old `flags.json` is retired).** Read every value through
[`flags.rs`](src/flags.rs) `state_flags()` / `bk_flags()`, never hardcoded shifts.

`flags_bk` refcounts (the holds the coarse apply path bumps): `position_hold_count`,
`slot_share_count` (shared `borrow`/`share`), `slot_hold_count` (exclusive
`claim`/`use`), `touch_count` (concurrent-recipe cap, max 3), plus `tile_stock_0`
/ `tile_stock_1` (2 bits each, promoted-tile stock) and the
`micro_is_card`/`stack_state`/`stack_index` placement fields.

## Surface bands
`Card.surface` / `Zone.surface` is the `surface` byte of `macro_zone` (bits
24-31). Constants in [shared/codec/src/packed.rs](../../../../shared/codec/src/packed.rs):
`1 = INVENTORY_LAYER` (soul bucket), `2 = PLAYER_INVENTORY_LAYER`,
`63 = MINI_ZONE_LAYER`, `64.. = WORLD_LAYER`. The split at `>= 32` ("carries tile
data") gates synthetic-hex derivation. `0` is the sentinel "absent."

## Stacks: root-pointer chains and `micro_location`
`Card.micro_location: u32`, dual-interpreted by `micro_is_card` (`flags_bk` bit 24):
- **SET** → the **root** card's id (flat chain; every member points at the root).
  The btree index makes "all members of root R" one filtered scan. Branch =
  `stack_state` (0=hex/tile, 1=up, 2=down, 3=deferred); slot = `stack_index` (0..15).
- **CLEAR** → packed loose coords `[local_q:3 | local_r:3 | x:12 | y:12 | rsvd:2]`.

Written via the shared `card_model::Micro` model so the discriminator and value
never drift. Tile cards are *just* cards — a loose tile-card is `micro_is_card`
clear at `(local_q, local_r)`. Design: [docs/micro_location_rewrite/](../../../../docs/micro_location_rewrite/00_design_and_plan.md).

## Time discipline (`client_time_ms` + grace)
Client-invoked state-writing reducers take `client_time_ms` first (after `ctx`);
the server resolves `cards::effective_now_ms(ctx, client_time_ms)?` (`min(client,
server)` within asymmetric grace — back `BACKWARD_GRACE_MS = 10_000`, forward
`TIME_DRIFT_BUFFER_MS = 2_000`, else `Err("time_drift:client_(behind|ahead)_by=…")`)
and threads `now_ms` everywhere via the `_at` write variants.

- **Grace-applying** (client-invoked): `place_card`, `move_soul`, `add_card`,
  `ensure_region`, `request_zone`, `spawn_soul`.
- **Gate-stamped** (the coarse apply path): `apply_action` / `apply_action_tile`
  take explicit `now_ms` + `completion_ms` from the gate (which already resolved
  the action's clock) and trust them — authz is the gate's job.
- **Admin / scheduled** (`gc_sweep`, `init`, `set_shard_identity`) use raw
  `now_ms(ctx)` / no clock.

## SpacetimeDB 2.x macro syntax (migration trap)
- `#[table(accessor = cards, public)]` — `accessor = <ident>` is the 2.x form
  (not 1.x `name = "..."`). Scheduled tables: `#[table(accessor = …, scheduled(<reducer_ident>))]`
  + a `scheduled_at: ScheduleAt` column.
- Calling another file's table accessor needs the trait in scope: `use crate::cards::cards;`
  (often aliased `as _` to silence the unused-name warning).
- Non-table reducer-arg structs need `#[derive(SpacetimeType)]`. Reducer args over
  the HTTP `/call` path ride as named JSON fields; `Vec<T>` → JSON arrays (the
  coarse reducers pass parallel primitive Vecs for this reason).
- `#[auto_inc]` only on the `#[primary_key]` column.

## Pitfalls
- **Don't hand-roll `valid_at`** — `create`/`update_with(_at)` stamp it via
  `pack_valid_at(time_ms, next_sequence(ctx))`.
- **Use `prior_at`, not `latest`, when writing past `now`** — a future-stamped row
  is invisible to `latest()` by definition.
- **`update_with` returns `None` if no row exists** — spawn via `create(_at)` first.
- **Don't OR holds onto a card by hand** — use the `acquire_*`/`release_*` helpers
  (or go through `apply_action`); raw bit-sets skip forward-prop and corrupt the
  overlap (refcount) semantics.
- **The build runs in a container** — error paths read `/workspace/server/modules/shard/…`
  → `spacetime/server/modules/shard/…` on the host.
- **Bindings are generated, not edited** — fix the Rust schema and rerun `bin/st build shard`.
