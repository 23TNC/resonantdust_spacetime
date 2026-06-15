# AGENTS.md — `players` module (auth / directory)

## Purpose
SpacetimeDB 2.1.0 server module (Rust → wasm32), crate `resonantdust_players`.
The **auth + player-directory** database — low-write, so one instance serves all
players (sharding players is deferred; don't add it without need). It holds the
player roster, profiles, and login/session state; **it does not hold gameplay
state** — cards/souls/world live in the [`shard`](../shard/AGENTS.md) data module.

The link between them: `Player.data_shard` tells the client which **cards** shard
to subscribe to after login (mirroring how a planned region-index would map a
region → its region shard). Publishes to `resonantdust-<env>-players-N`.

> **History note.** Older docs/memory describe a giant "shard module" here that
> ran the whole monolith (`propose_action`, `action_completion`, `world_gen`,
> `mini_zone`, player-dimensions, character-creation, …). That predates the
> gate-authority pivot and the module split; none of it lives here now. The
> current data + action surface is the [`shard`](../shard/AGENTS.md) module +
> the gate.

## Build & iterate
- `bin/st build players` (or `bin/st build` for every module) — builds in the
  Docker build container, regenerates TS + gateway bindings into `bindings/players/`.
- `bin/st re players` wipes + republishes to `resonantdust-<env>-players-0`.
  `bin/st sql players ...` / `bin/st select players ...` for live queries.
- Don't reach for host `cargo` / `spacetime`.

## Module map
| File | Role |
| --- | --- |
| [src/players.rs](src/players.rs) | `players` public history table + private `player_sessions` (Identity → player_id) + public `player_profiles` (one row per player; `starter_packs` unlock bits, lifecycle-summary fields) + private `player_id_counter`. Reducers: **`claim_or_login(_client_time_ms, name)`**, **`set_last_login(_client_time_ms)`** — both **grace-exempt** (they bootstrap the client's time-offset window and back-shift their writes by `TIME_DRIFT_BUFFER_MS`). Helpers: `validate_player_name`, `resolve_caller`, `next_player_id` (reserves ids `< FIRST_PLAYER_ID = 1024` for system/world pseudo-players), `create_at` / `update_with_at`. Carries its own copy of the bitemporal write skeleton (`write_at` / `update_with_at`) and the time-discipline constants — a deliberate mirror of `shard::cards` (see *valid_at* in the shard doc). |
| [src/gate_api.rs](src/gate_api.rs) | Gate-facing reducers: `set_player_faction`, `set_player_permissions`. Trust their args (authz is the gate's job). |
| [src/gc.rs](src/gc.rs) | Single recurring `gc_schedule` + `gc_sweep` (seeded by `init`) — prior-version reap over `players` / `player_profiles`. |
| [src/sequence.rs](src/sequence.rs) | Private `sequence_counter` + `next_sequence()` — same load-bearing `valid_at` disambiguator every history table uses. |
| [src/packed.rs](src/packed.rs) | Thin `pub use resonantdust_data::packed::*;` re-export. |

## The `valid_at` pattern
`players` / `player_profiles` use the same versioned-row scheme as the data
module (`valid_at = (time_ms << 16) | sequence`, latest `valid_at_time ≤ now`
wins). The full contract — `prior_at` vs `latest`, same-ms purge, future-stamped
rows, sequence uniqueness — is documented once in
[shard/AGENTS.md](../shard/AGENTS.md#the-valid_at-pattern). This module's
`write_at` is a hand-mirrored copy of that skeleton (no forward-prop — player
rows don't need it); keep it in sync if the skeleton changes.

## Login flow
`claim_or_login` (here) → the `Player` row gives `data_shard` (the assigned cards
shard, `0` today) → the client subscribes to that cards shard and queries
`cards where owner_id == player_id` to find its top-level `player_soul` cards
(identified by ownership + the `is_owned_by_player` flag, **not** a reserved id
band — a player can own several). If none, the client calls `spawn_soul` (a
`shard`-module reducer) to mint one. No `soul_id` is stored on `Player`.

## Pitfalls
- **Grace-exempt reducers ignore `client_time_ms`** and back-shift their writes by
  `TIME_DRIFT_BUFFER_MS` so the row's `valid_at` matches the buffered
  `serverNowMs()` the client reads as the offset window seeds.
- **No `soul_id` on `Player`** — souls are found by the owner query (supports
  multi-character). Don't re-add it.
- **Don't shard this DB** without a real need — it's low-write by design.
- **Bindings are generated** — fix the schema and rerun `bin/st build players`.
