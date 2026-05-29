# AGENTS.md — shard module

## Purpose
SpacetimeDB 2.1.0 server module (Rust → wasm32). Authoritative gameplay state: the world (`cards`, `zones`), the soul mirror (`souls`), the player roster (`players`, `player_profiles`, `lifecycle_pending`), and the recipe / movement pipelines. Publishes to `resonantdust-<env>-shard` (see [`spacetime.<env>.json`](spacetime.dev.json) — `--env <env>` to switch). Compiles and runs inside a Docker-hosted SpacetimeDB instance — never against a host `spacetime` CLI.

Chat is a **separate module** ([../chat/AGENTS.md](../chat/AGENTS.md)) with its own database, queue, and wasm; nothing in this module references it.

Lifecycle-pending cards (magnetic anchors AND on_create-style decay cards) share one mechanism: a `lifecycle:` block on the card def stamps the `magnetic` state flag at creation via definition-flag inheritance, populates a `lifecycle_pending` detail row, and waits for the client to submit the declared recipe via `propose_action`. There is no dedicated tick module — resolution is client-driven and the periodic GC sweep is the integrity backstop. The flag is still spelled `magnetic` (bit 2) in [`content/cards/flags.json`](../../../../content/cards/flags.json); a rename to `lifecycle_pending` is still pending.

> **Recovery note (2026-05).** This module was rolled back to an earlier snapshot after a data loss. Three modules that older docs/memory describe are **gone and staying gone**: `stacks.rs` (`CardStack` / `submit_action`), `character_creation.rs` (`create_character`), and `player_dimension.rs` (`enter/exit_player_dimension`, `PLAYER_DIMENSION_LAYER`, `create_player_dimension`, `latest_for_owner`, the `owner_id_filter` threading). Player-scope blueprints (card_type 3, `request_player_blueprint`, the `soul_info`/`blueprint_info` profile nibbles) were also cut. Don't treat their absence as a bug. A few *surviving* features lost wiring in the same rollback — see the ⚠ markers below.

## Build & iterate
- **Always** use `bin/st build shard` (or `bin/st build` to do every module). The wrapper runs [`content/gen-ids.py`](../../../../content/gen-ids.py), then `cargo build --release --target wasm32-unknown-unknown` inside the build container with workdir set to `/workspace/server/modules/shard`, then regenerates TS bindings into [`../../../../pixijs/src/server/spacetime/bindings/shard/`](../../../../pixijs/src/server/spacetime/bindings/shard/). Don't reach for `cargo`, host `spacetime`, or ad-hoc `docker compose run` — even for introspection.
- `bin/st publish shard` / `bin/st republish shard` push the wasm to the running `start` container. The CLI loads [`spacetime.json`](spacetime.json) (anchor file — empty but its existence pins the CLI in this directory) plus [`spacetime.<env>.json`](spacetime.dev.json) for db name / server. `bin/st sql shard ...`, `bin/st select shard ...`, `bin/st call shard <reducer>` for live queries. See [`bin/st`](../../../../bin/st).
- **One-shot refresh after content / schema edits: `bin/st re`** — chains `content wasm; st build shard; st delete shard; st bootstrap`. Use this when JSON catalogs change, the Zone schema changes, or you want a clean slate. `bin/st re all` rebuilds every module; `bin/st re <module>` scopes to one.
- TS bindings are generated; private tables (no `public`) are skipped during codegen. `player_sessions`, `sequence_counter`, `card_id_counter`, `lifecycle_pending`, `pending_actions`, and `gc_schedule` are intentionally private.

## Module map
| File | Role |
| --- | --- |
| [src/lib.rs](src/lib.rs) | module list — every new file needs a `pub mod` line. The 19 declared modules match the files on disk exactly. |
| [src/cards.rs](src/cards.rs) | `cards` public table + most bookkeeping: `latest` / `prior_at` / `delete_at`, `create(_at)` / `update_with(_at)` / `write_at` (every write fires `souls::on_card_write` plus the lifecycle install/cleanup hooks; retention handled by the periodic GC sweep), `next_card_id` (private `card_id_counter` table), the `acquire_<name>` / `release_<name>` / `propagate_<name>_forward` triplet for each refcount in `flags_bk` (`position_hold`, `slot_share`, `drop_hold`, `slot_hold`, `touch`, `server`) via the `decl_count_ctx!` macro — see [flags.json](../../../../content/cards/flags.json) for the field layout, `scrub_or_repath_position_forward` / `propagate_flag_diff_forward` (dirty/preserve forward-prop, `flags_state` only), owner-walk helpers `owning_player` / `owning_soul`, the `set_stacked` / `set_loose` micro_location writers, the tile-card promotion/demotion primitives `find_or_create_tile_card` / `find_tile_card_at` / `tile_def_id_view` / `tile_full_view` / `tile_stock` / `set_tile_stock`, plus `set_*` field setters and definition-flag inheritance (`definition_flag_mask`, which stamps the `magnetic` flag on lifecycle-pending cards at creation). **Also owns the time-discipline contract:** `TIME_DRIFT_BUFFER_MS` / `MAX_RTT_MS` / `BACKWARD_GRACE_MS` constants, `WORLD_PLAYER_ID`, and `effective_now_ms(ctx, client_time_ms)`. |
| [src/zones.rs](src/zones.rs) | `zones` public table (history-style, same `valid_at` shape) — `latest`, `latest_for(surface, macro_zone)`, `next_zone_id`. Tile fields `t0..t15` (16 u64) carry 64 per-tile u16 slots in the layout `[def_id:u12 \| stock0:u2 \| stock1:u2]`. Tile setters: `set_tile_at(zone_id, time_ms, row, col, def_id, stock0, stock1)` writes a full slot with forward-prop, `set_tile_rows` is the bulk worldgen writer. Under the tile-as-card model the cards table is consulted first by `cards::tile_def_id_view` / `tile_full_view`; only at-rest tiles live in the packed zone slot. `Zone.owner_id` is overloaded as a `card_id` (mini_zone anchor). Reaped by the GC sweep. *(Note: `zones::next_zone_id` is a scan-and-increment fallback shared with `world_gen`'s seeder — not a counter row.)* |
| [src/players.rs](src/players.rs) | `players` public history table + private `player_sessions` (Identity → player_id) + public `player_profiles` (one row per player; holds `starter_packs` unlock bits, `lifecycle_count` / `earliest_lifecycle_expires_ms` summary fields, and `blueprints_0` u64 player-scope discovery bitfield). Reducers: `claim_or_login(_client_time_ms, name)`, `set_last_login(_client_time_ms)`, `client_disconnected`. The two write-reducers are **grace-exempt** — they bootstrap/reseed the client's offset window and back-shift their row writes by `TIME_DRIFT_BUFFER_MS`. Helpers: `validate_player_name`, `resolve_caller`, `delete_player` cascade, `next_player_id` (reserves ids `< FIRST_PLAYER_ID = 1024` for system/world pseudo-players), `resync_lifecycle_summary`, `create_at` / `update_with_at`. |
| [src/souls.rs](src/souls.rs) | `souls` public follower table — one row per soul card, mirrors position + packs four `u8` counters (corpus / anima / sollertia / aether) into `stats` / `fatigued` / `injured` u32 quads. Also owns the public `soul_privates` flat table (`card_id` PK, `blueprints_0` u64 soul-scope blueprint discovery bitfield, `active_blueprints` u8). **`on_card_write` is the single sync point**: `cards::write_at` calls it on every write and diffs prior vs new to (1) mirror soul-card position, (2) apply faculty-card stat deltas to the owning soul, (3) maintain `SoulPrivate.active_blueprints` for soul-scope blueprint cards via owner-chain walk. ⚠ The owner-chain walk uses unbounded `owning_soul` (`latest()`); the `owning_soul_at` time-bounded sibling exists but is not wired in — a known regression for future-stamped chains. Public reader `soul_max_for_player`. |
| [src/blueprints.rs](src/blueprints.rs) | `request_blueprint(client_time_ms, soul_card_id, blueprint_id, surface, macro_zone, micro_location, ...)` — soul-scope only (catalog `BlueprintScope::Soul`; discovery bit on `SoulPrivate.blueprints_0`; cap from the soul def's `aspects.builder`; placed card owned by the soul). Relies on `souls::on_card_write` for delta bookkeeping — release happens implicitly when the card goes dead. *(Player-scope `request_player_blueprint` was cut — see the recovery note.)* |
| [src/lifecycle_pending.rs](src/lifecycle_pending.rs) | private `lifecycle_pending` table (PK `card_id`, btree `player_id`, `expires_at_ms`). Populated by `cards::write_at` when a lifecycle-pending card is created via def-flag inheritance, cleaned up on the dead-bit transition. Owns the `block_check` resolution-gate helper. ⚠ **Only `deploy_mini_zone` currently calls `block_check`** — the documented `propose_action` / `add_card` / `request_blueprint` call sites lost their wiring in the rollback. |
| [src/gc.rs](src/gc.rs) | Periodic GC sweep — single recurring `gc_schedule` row + `gc_sweep` reducer fires every 10 minutes (seeded by `init`). Walks `cards` / `players` / `souls` once each, applies player-aware retention: prior versions always reaped; dead cards retained until owner has been logged in 5+ minutes (or 5 min for world-owned, or 30-day hard cap for abandoned). Also calls `pending_actions::sweep_stale`. Replaces the retired per-write `schedule_delete_*` model. |
| [src/pending_actions.rs](src/pending_actions.rs) | private `pending_actions` table — in-flight `propose_action` dedup registry keyed on `dedup_key = hash(recipe_id, root, bindings)`. `install` at the end of propose validation, `is_in_flight` for the duplicate-rejection branch, `release` in `action_completion::commit`, `sweep_stale` (orphan reap) from the GC sweep. |
| [src/flags.rs](src/flags.rs) | **Canonical server-side flag field-routing layer.** `state_flags()` / `bk_flags()` lazily load every `cards_state` / `cards_bk` mask + shift + max from the content crate's `flags_core` registry (source of truth: [`content/cards/flags.json`](../../../../content/cards/flags.json)) and cache them in `OnceLock`. Replaces the per-module hand-rolled `const FLAG_*`. A missing flag panics at first access (build/test-time mismatch signal). `TOUCH_COUNT_CLIENT_CAP` / `SERVER_COUNT_CAP` constants live here. **Read flag values through these helpers, not hardcoded shifts.** |
| [src/recipe_eval.rs](src/recipe_eval.rs) | ⚠ **Effectively dead.** Only `soul_stack` remains and it has zero callers; the rest of the historical predicates were removed. The propose-time matcher/predicates now live inline in `actions.rs`. Slated for deletion. |
| [src/actions.rs](src/actions.rs) | `propose_action(client_time_ms, recipe_id, surface, macro_zone, micro_location, root, bindings: Vec<Vec<u32>>)` public reducer — verifies a client-resolved recipe match under the recipe-tape model. The client walks recipes and pre-resolves every `slot.<branch>.<index>` binding to a concrete `card_id`; the server verifies via direct array index plus O(1) transition checks. Stages: (1) walk the tape, resolve `Seg::Slot` via `bindings`, evaluate input predicates with synthetic-tile fallback for branch 0; (2) existence / liveness / ownership / hold-count checks; (3) chain-stitch top-level iterator bindings; (4) `apply_locks` (acquire `touch` per card + `slot_hold` / `slot_share` / `position_hold` per kind), register in `pending_actions`, then invoke `action_completion::plan` + `commit` synchronously with `completion_ms = now_ms + duration_secs*1000`. Owns the local consts `WORLD_PLAYER_ID`-adjacent `SYNTHETIC_HEX_MIN_SURFACE`, `TILE_CARD_TYPE`. |
| [src/action_completion.rs](src/action_completion.rs) | `plan()` + `commit()` — NOT reducers; invoked synchronously from `propose_action`. `plan()` walks the recipe's output tape into a `TapeWalker` (`vars: [i32; 8]`, `duration`, per-card `styles`, `pending: Vec<Effect>`) and returns the plan + the resolved `HoldKinds` map. `commit()` emits every `Effect` as a future-stamped row at `completion_ms` (reagents flip dead, products spawn via `cards::create_at`, tile-stock writes via `cards::set_tile_stock`), releases locks (`release_touch` / `release_slot_hold` / `release_slot_share` / `release_position_hold`), and `pending_actions::release`s the dedup row. Supports `sys.duration.set`, `<path>.style.set`, `var.N.set/add/sub`, `when.<pred>.<inner>` gates, `<path>.destroy`, `<path>.create: <def_id>`, `<path>.aspect.X.sub/add/set` (tile-stock). Uses `crate::packed::WORLD_LAYER`. |
| [src/place.rs](src/place.rs) | `place_card(client_time_ms, card_id, placement: Placement)` public reducer — unified "place a card" primitive that subsumed `equip_card` / `unequip_card`. `Placement` (flat-encoded, `kind` discriminator `PLACEMENT_STACK` / `PLACEMENT_LOOSE`) covers stack-onto-root in a direction (UP / DOWN / HEX) and loose-place at an explicit address. Validates source eligibility (alive, caller-owned, no in-flight slot/share/position holds on source or descendants), resolves the target, then writes the source row + re-stamps descendants' `surface` / `macro_zone` to match the new destination. **`owner_id` is deliberately never touched** by placement — ownership is independent of position; there is no ownership-transfer reducer today. |
| [src/movement.rs](src/movement.rs) | `move_soul(client_time_ms, soul_id, path: Vec<TilePoint>)` reducer — client submits a precomputed A* path; server validates per-step adjacency + traversability + length cap (`MAX_VALIDATION_STEPS = 256`), then writes a sequence of future-stamped soul rows along the path. Reads tile cost from the `cost` trait, soul speed from the `speed` trait. Tile lookups route through `cards::tile_def_id_view` so promoted tile-cards win; mini_zone overlays consulted via `mini_zone::anchor_covering_hex`. In-flight moves are interrupted by rewriting future rows from `now` (respects `position_preserve`-pinned rows as stop conditions). |
| [src/mini_zone.rs](src/mini_zone.rs) | `deploy_mini_zone` / `pickup_mini_zone` reducers — radius-3 hex disk overlaid on the world, backed by a `Zone` row at `(surface = MINI_ZONE_LAYER (63), macro_zone = anchor.card_id)`. `deploy_mini_zone` runs `lifecycle_pending::block_check`. `anchor_covering_hex` / `tile_at_anchor` are used by `actions::derive_synthetic_hex` and `movement` so anchored mini_zones occlude the world tiles underneath them. |
| [src/world_gen.rs](src/world_gen.rs) | Procedural terrain — fBm value-noise → biome lookup → tile def. `generate_zone_tiles(macro_q, macro_r, seed)` is the pure tile builder. `biome_for(global_q, global_r)` is the revert target used by `action_completion` when a synthetic-hex tile is consumed. `WORLD_SEED` constant. `generate_forest_terrain(seed, radius)` reducer + the helper called from `utilities::bootstrap`. *(Carries a local `WORLD_SURFACE = 64` that duplicates `packed::WORLD_LAYER` — flagged for cleanup.)* |
| [src/utilities.rs](src/utilities.rs) | Reducers: `bootstrap` (admin/debug seed via `world_gen::generate_forest_terrain`), `add_card(client_time_ms, soul_card_id, card_key)` (inventory spawn for the caller's soul). Equip / unequip are gone — use `place_card`. |
| [src/sequence.rs](src/sequence.rs) | private `sequence_counter` single-row table + `next_sequence()` u16 allocator. Read-modify-write each call; wraps at 65536. **Load-bearing for the `valid_at` scheme** — every history-table write asks for a fresh sequence to disambiguate same-ms PKs. Chat has its own independent counter. |
| [src/packed.rs](src/packed.rs) | bit-packing helpers — thin re-export shim. The single source of truth is [`content/src/packed.rs`](../../../../content/src/packed.rs); this file just `pub use resonantdust_content::packed::*;`. Tests live in the content crate (`bin/content test`). |
| (registries) | card / aspect / recipe / blueprint registries live in the [`resonantdust-content`](../../../../content/) crate (path-dep, see `Cargo.toml`); call into `resonantdust_content::definition_core::*`, `recipe_core::*`, `blueprint_core::*`, `flags_core::*`, `packed::*`. |

## The `valid_at` pattern
`cards`, `zones`, `players`, and `souls` share one primary-key shape (chat's `chat_messages` uses the same shape independently in its own module):

```
valid_at: u64 = (time_ms: u48 << 16) | sequence: u16
```

- **`valid_at` is the primary key**; the row's logical id (`card_id` / `zone_id` / `player_id`, etc.) is a `#[index(btree)]` column. Two rows for the same id form a *history* ordered by `valid_at_time`.
- **Time is milliseconds.** Every module defines a local `fn now_ms(ctx) -> u64 = (ctx.timestamp.to_micros_since_unix_epoch() / 1_000) as u64`. The u48 ms slot has ~8920 years of runway.
- **The sequence is the source of PK uniqueness across writes at the same millisecond.** [`sequence::next_sequence(ctx)`](src/sequence.rs) hands out a fresh `u16` per call.
- **Same-(id, time_ms) writes** are explicitly purged before insert. `cards::write_at` calls `delete_at(ctx, card_id, time_ms)` so "last write at this (id, ms) wins" — needed for in-reducer accumulation patterns (e.g. `souls::apply_slot_delta` firing N times at one `now_ms`).
- **Future-stamped rows are first-class.** Recipe completions and movement step queues are stamped at a `valid_at_time > now`. `latest()` filters `valid_at_time ≤ now_ms` (max-by-key among those) — it does NOT just take the deepest row. `cards::prior_at(ctx, card_id, time_ms)` is the parameterised form used by writers that want "the row current at the time we're writing."
- **`latest()` and `prior_at()` are not interchangeable.** A caller that has stamped a future row reaches the new state via `prior_at(..., future_time)`, never `latest()`. Recipe products spawned by `action_completion::commit` at `completion_ms` exist *only* at that future stamp.
- **No `#[unique]` on non-PK columns.** Cross-row uniqueness (e.g. unique player names) is checked via `#[index(btree)]` + lookup in the reducer before insert. Reducers are serialized per-database, so lookup-then-insert is race-free.
- **`#[auto_inc]` only on the PK.** Cards get ids from `cards::next_card_id` (private `card_id_counter` single-row table — *not* `content/gen-ids.py`, which only allocates `def_id`s); zones via `zones::next_zone_id` (scan-and-increment); players via `players::next_player_id`.

Client mirror: [pixijs/src/server/data/packing.ts](../../../../pixijs/src/server/data/packing.ts) and [pixijs/src/server/data/ValidAtTable.ts](../../../../pixijs/src/server/data/ValidAtTable.ts). Schema changes here ripple through bindings and require updates over there.

## The `flags` columns
`Card` carries **two** flag hosts, both `u32`: `flags_state` (what is true about the card; forward-propagated by `propagate_flag_diff_forward` in `cards::write_at`) and `flags_bk` (bookkeeping — refcount holds + dirty/preserve markers + tile-card stock + the `micro_location` discriminator/stack fields; never bit-diff propagated). **Bit positions are pinned by [`content/cards/flags.json`](../../../../content/cards/flags.json) — that file's per-bit descriptions are canonical.** Read every value through the [`flags.rs`](src/flags.rs) `state_flags()` / `bk_flags()` helpers, never hardcoded shifts.

**`flags_state` (state bits):**

| Bits | Name | Notes |
| --- | --- | --- |
| 0 | `dead` | Server marked the card consumed. Set via UPDATE so the row carries a death `valid_at` the client times its dying animation against. |
| 1 | `pos_need` | Server *requires* this row's `micro_location` exactly — client splices the card into the slot, re-anchoring the prior occupant up the chain (new card wins). Set on chain-stitch rows; cleared at completion. |
| 2 | `magnetic` | Lifecycle-pending marker. Stamped at creation for any def with a `lifecycle:` block via definition-flag inheritance. Cleared when the resolving recipe completes. Name pending rename to `lifecycle_pending`. |
| 3 | `surface_locked` | Static — card may not change surface. |
| 4 | `is_owned_by_player` | Disambiguates `Card.owner_id`. SET → `owner_id` is a `player_id` (soul cards). CLEAR → `owner_id` is a `card_id` (container; `0` = world). [`cards::owning_player`](src/cards.rs) walks until it hits this bit. |
| 5..=7 | `progress_style` (u3) | Progress-bar style for the row's "next future event" client render. 0 = none, 1 = LTR/CW, 2 = RTL/CCW. |
| 8..=11 | `portrait_id` (u4) | Soul-card portrait selector. |
| 12 | `pos_want` | Server *prefers* this row's `micro_location` but does not require it — on conflict the incoming card stacks ON TOP (existing card keeps the slot). Advisory sibling of `pos_need`. |
| 13 | `zone_born` | Card was materialized from zone tile data (a promoted tile-card). Set once at `find_or_create_tile_card`, never flipped. |

**`flags_bk` (bookkeeping):**

| Bits | Name | Notes |
| --- | --- | --- |
| 0 | `position_dirty` | Auto-set when the new row's position fields differ from prev. Server-managed. |
| 1 | `position_preserve` | Caller intent: future-prop must not overwrite this row's position. Movement scrub stops at the first row carrying it. |
| 2 | `data_dirty` | Auto-set when `flags_state` / `owner_id` / `packed_definition` differ from prev. Server-managed. |
| 3 | `data_preserve` | Caller intent: future-prop must not overwrite this row's data. `propagate_flag_diff_forward` skips it. |
| 4..=6 | `position_hold_count` (u3) | Refcount; `> 0` means position-pinned. Permanent locks via the "never-released +1" idiom. |
| 7..=9 | `slot_share_count` (u3) | Refcount of shared (`borrow.` / `share.`) holds. |
| 10..=12 | `drop_hold_count` (u3) | Refcount; `> 0` blocks stacking onto this card as a child. ⚠ No `acquire_drop_hold` call site exists today — the count is currently always 0. |
| 13..=15 | `slot_hold_count` (u3) | Refcount of exclusive (`claim.` / `use.`) holds. |
| 16..=17 | `touch_count` (u2, max 3) | Cap of client-initiated recipes touching this card concurrently. `validate_bindings` rejects a 4th. |
| 18..=19 | `server_count` (u2, max 3) | Cap of server-internal touches. ⚠ No `acquire_server` call site exists today — the count is currently always 0. |
| 20..=21 | `tile_stock_0` (u2) | Stock slot 0 for promoted tile-cards. Seeded from the Zone on `find_or_create_tile_card`, written by the `ModifyTileStock` effect, folded back on GC demotion. |
| 22..=23 | `tile_stock_1` (u2) | Sibling of `tile_stock_0`. |
| 24 | `micro_is_card` | Discriminates `micro_location`: SET → root card_id (stack member); CLEAR → packed loose coords + offset. Kept in `flags_bk` so it carries forward in lockstep with `micro_location`. |
| 25..=26 | `stack_state` (u2) | Gated on `micro_is_card`. Stacked: 0=hex, 1=top, 2=bottom, 3=deferred (`STACK_DIR_*` / `STACK_STATE_DEFERRED`). Loose: 0=loose-hex, 1=loose-rect, 2=snap-hex, 3=snap-rect (`LOOSE_*` / `SNAP_*`). |
| 27..=30 | `stack_index` (u4) | Slot index within a branch (0..15) when `micro_is_card` is set. Gap-tolerant; placement claims the next free index, fails over to loose on overflow. |

**Helpers.** Acquire/release/propagate triplets exist for each refcount in `flags_bk` via the `decl_count_ctx!` macro in [`cards.rs`](src/cards.rs). Use these (or the high-level `acquire_position_hold`, `acquire_slot_hold`, etc.) rather than hand-rolling field arithmetic. Permanent locks are `acquire_<name>` at spawn with no matching release. Definition-level flags inherit on `create` / `create_at` via `definition_flag_mask` — don't OR them by hand.

**Client-side drag holds** live in a `DragHoldStore` sidecar inside `pixijs/src/game/input/DragManager.ts` — the server has no concept of an in-flight client drag.

## Surface bands
`Card.surface` and `Zone.surface` are the `surface` byte of `macro_zone` (bits 24-31, read via `packed::surface_of`). Constants live in [content/src/packed.rs](../../../../content/src/packed.rs):

| Band | Constant | Purpose |
| --- | --- | --- |
| `0` | (reserved) | Sentinel "absent." Don't write here. |
| `1` | `INVENTORY_LAYER` | A soul's inventory bucket. `macro_zone payload = soul.card_id`. No hex tile data. |
| `2` | `PLAYER_INVENTORY_LAYER` | A player's account-wide inventory bucket. `macro_zone payload = player_id`. No hex tile data. |
| `3..32` | (reserved) | Future personal / panel surfaces. |
| `32` | `POCKET_DIMENSION_LAYER` | Card-anchored pocket dimension. `macro_zone payload = anchor.card_id`. Reserved — no reducer creates these yet. |
| `33..63` | (reserved) | Future tile-bearing surfaces. |
| `63` | `MINI_ZONE_LAYER` | Mini_zones — radius-3 hex disks overlaid on the world. `macro_zone payload = anchor.card_id`. Backed by a real `Zone` row. |
| `64..` | `WORLD_LAYER` | World tiles. `macro_zone payload = packed (zone_q, zone_r)`. Synthetic hexes come from the underlying `Zone` tile slot; a covering mini_zone occludes. |

The split between "carries tile data" (`>= 32`) and "panel / inventory" (`< 32`) gates synthetic-hex derivation (`actions::SYNTHETIC_HEX_MIN_SURFACE`). `loose_kind_for_surface` (content `packed.rs`) picks `LOOSE_HEX` for world/mini-zone and `LOOSE_RECT` for container surfaces.

*(There is no `PLAYER_DIMENSION_LAYER` — that band was cut with the player-dimension feature. Surface 62 is unallocated.)*

## Stacks: root-pointer chains and `micro_location`
The `micro_zone` byte is **gone**. Everything it carried now lives in `Card.micro_location: u32`, dual-interpreted by the `micro_is_card` flag (`flags_bk` bit 24). Source of truth for the bit layout: [content/src/packed.rs](../../../../content/src/packed.rs); design rationale: [docs/micro_location_rewrite/00_design_and_plan.md](../../../../docs/micro_location_rewrite/00_design_and_plan.md).

- **`micro_is_card` SET — stack member.** `micro_location` is the **root** card's id (a flat chain — every member points at the root, not its immediate parent). The btree index on `micro_location` makes "all members of root R" a single filtered scan. The branch is the `stack_state` field (0=`STACK_DIR_HEX`/tile, 1=`STACK_DIR_UP`/top, 2=`STACK_DIR_DOWN`/bottom — values match recipe branch numbers; 3=`STACK_STATE_DEFERRED` for host-anchored followers tracked separately). The slot within the branch is `stack_index` (0..15, gap-tolerant, fail-to-loose on overflow).
- **`micro_is_card` CLEAR — loose card.** `micro_location` is packed coords + offset `[local_q:3 | local_r:3 | x:12 | y:12 | rsvd:2]` (`pack_micro_loose` / `pack_micro_snap`). `stack_state` encodes the loose kind (`LOOSE_HEX` / `LOOSE_RECT` / `SNAP_HEX` / `SNAP_RECT`). World cards, inventory loose cards, and at-rest tile-cards all live here.

Both interpretations are written via the `cards::set_stacked` / `cards::set_loose` helpers, which keep `micro_location`, `micro_is_card`, `stack_state`, and `stack_index` in lockstep. Hex / tile cards are *just* cards — a loose tile-card is `micro_is_card` clear with `(local_q, local_r)`; a recipe-bound tile-card becomes a stack member with `stack_state = STACK_DIR_HEX` under the chain root, and readers needing its hex parent-walk to the loose ancestor (see `cards::tile_full_view`).

## Action pipeline
Recipe execution under the recipe-tape model. One public reducer, no scheduler:

1. **Propose.** Client matches the recipe locally and submits `propose_action(client_time_ms, recipe_id, surface, macro_zone, micro_location, root, bindings)`. The server resolves `effective_now_ms(client_time_ms)?` once and threads `now_ms` everywhere:
   - **Stage 1 (input verify).** Walk `recipe.input`, resolve each `Seg::Slot { iter, offset }` via `bindings[iter][offset]`, evaluate predicates. `.owner` / `.parent` transition steps verify against the previously-resolved card. Branch 0 supports a synthetic-tile sentinel (`bindings[iter_0][0] == 0` → derive from zone tile); if it fires, the tile is promoted via `cards::find_or_create_tile_card` and substituted before any downstream stage.
   - **Stage 2 (existence / liveness).** Every `card_id` exists, alive, ownership chains back to caller, no foreign in-flight slot_hold.
   - **Stage 3 (chain-stitch).** Root lands loose at `(surface, macro_zone, micro_location)`; top-level iterator bindings are stitched into branch `iterator.branch` as flat root-pointer members. Nested-iterator bindings are left alone.
   - **Stage 4 (lock + schedule).** `apply_locks` acquires `touch` per card + `slot_hold` / `slot_share` / `position_hold` per binding kind; `pending_actions::install` registers the dedup row; `action_completion::plan` builds the output plan and `commit` emits it with `completion_ms = now_ms + duration_secs * 1000`.
2. **Apply.** [`action_completion::plan`](src/action_completion.rs) walks the recipe's output tape into a `TapeWalker`; `commit` emits all `Effect`s as future-stamped rows at `completion_ms` (reagents flip `dead`, products spawn via `cards::create_at`, tile-stock writes via `cards::set_tile_stock`), then releases every lock and `pending_actions::release`s the dedup row.

There is no server-side action table — the action's state is the cards it stamped, plus the private `pending_actions` dedup row (released at commit, orphan-reaped by GC). The `lifecycle_pending` table tracks ACTIVE lifecycle-pending cards.

## Lifecycle resolution gate
Lifecycle-pending cards are recipes with a time-windowed phase. Resolution is **client-driven** — the server stamps initial state, the client decides when to submit success / failure / decay recipes via `propose_action`.

State on the server:
- [`lifecycle_pending`](src/lifecycle_pending.rs) — private table, one row per active entry. Populated by `cards::write_at` when a card carrying the `magnetic` flag is first written. Removed when the dead bit flips.
- `PlayerProfile.lifecycle_count` + `earliest_lifecycle_expires_ms` — summary fields; the hot-path block check reads only these and short-circuits when zero.

Block-check gate ([`lifecycle_pending::block_check`](src/lifecycle_pending.rs)):
- If the caller has zero active entries, or all are within the 60-second grace window past `expires_at_ms`, the call proceeds.
- If past grace, the call is rejected unless `involved_card_ids` overlaps the caller's blocked rows (permissive carve-out: any reference to a blocked card is treated as a resolution attempt).
- Stale rows referencing non-existent cards are purged inline before the gate evaluates.

⚠ **Wiring gap:** by design the gate should run on `propose_action`, `add_card`, `request_blueprint`, and `deploy_mini_zone`. Today **only `deploy_mini_zone` calls it** — the other call sites were lost in the rollback. Re-wiring them is a tracked regression, not an intentional design.

Failure path: regular recipes whose root is the lifecycle-pending card. Every magnetic-style card must have an unconditionally-matchable failure recipe so a player can always resolve out. Decay-style cards (zero-slot lifecycle recipe) use the same recipe as both success and fallback.

## Scheduled tables
SpacetimeDB scheduled tables use `#[table(accessor = ..., scheduled(<reducer_ident>))]` and require a `scheduled_at: ScheduleAt` column. Two patterns in use:

- **Recurring GC sweep.** [`gc_schedule`](src/gc.rs) — single row, `ScheduleAt::Interval(10 min)`, seeded by the module's `init` reducer. Fires `gc_sweep`.
- **Future-stamped row, no scheduler.** `action_completion::commit` and `movement::move_soul` write future-stamped rows directly. The "schedule" is implicit in the row's `valid_at_time`; the client's `ValidAtTable.promote(buffered_now)` surfaces it when wall-clock catches up. This is the pattern most actions use.

## SpacetimeDB 2.x macro syntax (migration trap)
- `#[table(name = "cards", public)]` is the **1.x** form. In 2.x use `#[table(accessor = cards, public)]`. `accessor = <ident>` is required; `name = "<string>"` is an optional SQL-name override.
- Scheduled tables: `#[table(accessor = ..., scheduled(<reducer_ident>))]`; the struct must include `scheduled_at: ScheduleAt`.
- Calling another table's accessor from a different file requires the trait in scope: `use crate::cards::cards;` — the trait is named after the accessor. Several modules use the `as _cards_table` alias pattern.
- Non-table reducer-arg structs need `#[derive(SpacetimeType)]`.
- `#[reducer(client_disconnected)]` is wired to disconnect events (`players::client_disconnected`). `#[reducer(init)]` seeds the GC schedule (`gc::init`).
- `#[auto_inc]` only applies to the `#[primary_key]` column.

## Time
- `ctx.timestamp` is a `Timestamp` (microseconds since epoch). Each module's `now_ms` = `to_micros_since_unix_epoch() / 1_000`.
- Round-trip a `time_ms` into a schedule: `ScheduleAt::Time(Timestamp::from_micros_since_unix_epoch((time_ms as i64) * 1_000))`.
- Recipe durations in JSON are seconds; `completion_ms = start_ms + duration_secs * 1_000`.

## Time discipline (`client_time_ms` + grace)
Every public state-writing reducer takes `client_time_ms: u64` as its first arg after `ctx`. The client submits its `serverNowMs()` estimate; the server validates and uses the result for *all* in-reducer time reads.

- **[`cards::effective_now_ms(ctx, client_time_ms)`](src/cards.rs)** returns `min(client, server)` if within grace, else `Err("time_drift:client_(behind|ahead)_by=<N>")`. Asymmetric policy: back-grace `BACKWARD_GRACE_MS = 10_000` ms, forward-grace `TIME_DRIFT_BUFFER_MS = 2_000` ms. (`MAX_RTT_MS = 3_000` documents the RTT cheat budget folded into the back grace.) The client's `ActionManager` parses `time_drift:client_ahead_by=<N>` and retries after `N + 250 ms`.
- **`*_at(time_ms)` variants** on `cards.rs`, `players.rs`, `zones.rs`, `souls.rs` stamp `valid_at` at an explicit `time_ms`. Every reducer that resolved an `effective_now_ms` value uses the `_at` form.
- **Grace-exempt reducers.** `claim_or_login` and `set_last_login` accept `_client_time_ms` and ignore it; both back-shift their row writes by `TIME_DRIFT_BUFFER_MS` so the row's `valid_at` matches the buffered `serverNowMs()` the client reads as soon as the capture seeds the window.
- **Grace-applying reducers.** `propose_action`, `place_card`, `move_soul`, `add_card`, `deploy_mini_zone`, `pickup_mini_zone`, `request_blueprint`. Each resolves `let now_ms = cards::effective_now_ms(ctx, client_time_ms)?;` at the top and threads `now_ms` downstream.
- **Admin / scheduled reducers** (`bootstrap`, `generate_forest_terrain`, `gc_sweep`, `init`) don't take `client_time_ms` — they use raw `now_ms(ctx)`.

Client-side counterpart: [pixijs/src/server/spacetime/ReducerManager.ts](../../../../pixijs/src/server/spacetime/ReducerManager.ts) owns the `serverNowMs()` estimator.

**Pitfalls when adding a new reducer:**
- If it writes game state: route everything through `effective_now_ms` and `_at` variants. Don't sprinkle raw `now_ms(ctx)` calls inside the body.
- The `client_time_ms` arg must come **first** (after `ctx`).
- If it's an admin / debug entry point, leave it on `now_ms(ctx)` and document the exemption.

## Pitfalls
- **Don't hand-roll `valid_at`.** `cards::create` / `update_with` / `update_with_at` stamp it via `pack_valid_at(time_ms, sequence::next_sequence(ctx))`. There's no public API that takes a pre-built `valid_at`.
- **Use `prior_at(ctx, id, time_ms)`, not `latest`, when writing past `now`.** A future-stamped product is invisible to `latest()` by definition.
- **`update_with` returns `None` if no row exists for the id.** All `set_*` helpers inherit this. Spawn the card first via `create` / `create_at`.
- **Public vs private affects the wire.** `player_sessions`, `sequence_counter`, `card_id_counter`, `lifecycle_pending`, `pending_actions`, and `gc_schedule` are intentionally private.
- **Bindings are generated, not edited.** Fix the Rust schema and rerun `bin/st build shard`.
- **Don't OR holds onto a card without propagation.** Prefer the high-level `acquire_position_hold` / `release_position_hold` helpers.
- **`is_owned_by_player` is asymmetric** — set on soul cards, clear on everything else (including world-owned cards: `owner_id = 0` AND bit clear).
- **The build runs in a container** — error paths are `/workspace/server/modules/shard/...` → `spacetime/server/modules/shard/...` on the host.
- **`gen-ids.py` runs before cargo** under `set -euo pipefail`. If `bin/st build` halts before "Compiling", suspect the content crate, not your Rust. Cross-reference [content/AGENTS.md](../../../../content/AGENTS.md).
