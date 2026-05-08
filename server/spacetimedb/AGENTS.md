# AGENTS.md

## Purpose
SpacetimeDB 2.0.3 server module (Rust → wasm32). Authoritative state for `cards`, `zones`, and `players`, plus per-table scheduled-delete sweeps that keep history-style tables from accumulating stale versions, plus a wire-format `CardStack` IPC type that action reducers consume. Compiles and runs inside a Docker-hosted SpacetimeDB instance — never against a host `spacetime` CLI.

## Build & iterate
- **Always** use `bin/st build` from the repo root. It runs [`content/gen-ids.py`](../../../content/gen-ids.py), a Dockerized `cargo build --release --target wasm32-unknown-unknown`, then a `bindings` container that regenerates [`../../pixijs/src/server/bindings/`](../../../pixijs/src/server/bindings/). Don't reach for `cargo`, host `spacetime`, or ad-hoc `docker compose run` — even for introspection.
- `bin/st publish` / `bin/st republish` push the wasm to the running `start` container. `bin/st sql ...`, `bin/st select ...`, `bin/st call <reducer>` for live queries. See [`bin/st`](../../../bin/st).
- TS bindings are generated; private tables (no `public`) are skipped during codegen. `schedule_delete_cards` is intentionally private.

## Module map
| File | Role |
| --- | --- |
| [src/lib.rs](src/lib.rs) | module list — every new file needs a `pub mod` line |
| [src/cards.rs](src/cards.rs) | `cards` table + helper API (`create`, `set_*`, `update_with`). All writes route through the private `write()` which stamps `valid_at` and enqueues a delete schedule |
| [src/zones.rs](src/zones.rs) | `zones` table — same `valid_at` pattern as cards, indexed by `zone_id` |
| [src/players.rs](src/players.rs) | `players` table (history-style, same `valid_at` pattern), private `player_sessions` table mapping `Identity → player_id`, plus `claim_or_login` / `client_disconnected` reducers, `validate_player_name`, `resolve_caller`, `delete_player` cascade |
| [src/stacks.rs](src/stacks.rs) | `CardStack` wire-format struct (NOT a table — `#[derive(SpacetimeType)]`) and `apply()` helper that writes a stack's positioning into the cards table. Consumed by action reducers |
| [src/packed.rs](src/packed.rs) | bit-packing helpers for `valid_at`, `macro_zone`, `micro_zone`, tile rows, recipe IDs. Pure, unit-tested, no SDK deps |
| [src/schedule_delete_cards.rs](src/schedule_delete_cards.rs) | private scheduled table + `delete_cards` reducer. `enqueue()` is called from `cards::write` |
| [src/schedule_delete_players.rs](src/schedule_delete_players.rs) | private scheduled table + `delete_players` reducer. `enqueue()` is called from `players::write`. Mirror of `schedule_delete_cards` |
| (no `src/definitions.rs`) | card / aspect / recipe registries live in the [`resonantdust-content`](../../../content/) crate (path-dep, see `Cargo.toml`); call into it as `resonantdust_content::definition_core::*` and `resonantdust_content::recipe_core::*` |

## The `valid_at` pattern
`cards`, `zones`, and `players` all share one primary-key shape:

```
valid_at: u64 = (id: u32 << 32) | time_secs: u32
```

- `valid_at` is the **primary key**; `card_id` / `zone_id` / `player_id` is a `#[index(btree)]` column.
- Writes never `UPDATE` — they `INSERT` a fresh row stamped with `(id, now_secs)`. The btree filter on the id column then yields a *history* of versions; `latest()` picks the row with the largest `valid_at_time`.
- `packed::pack_valid_at(id, secs)` / `valid_at_time(v)` / `valid_at_card_id(v)` work for all three tables despite the `card_id`-flavored parameter names — the bits don't care which table they came from.
- Two writes within the same wall-clock second collide on the primary key. `write()` deletes the existing row first, so "last write within the second wins" without surfacing a duplicate-key error.
- Old versions are pruned asynchronously by the scheduled-delete sweep below.
- **No `#[unique]` on non-PK columns.** Cross-row uniqueness (e.g. unique player names) can't be enforced via `#[unique]` because every version row of the same id would collide. Use `#[index(btree)]` and enforce uniqueness in the reducer via `latest_by_*()` lookup before insert. Reducers are serialized per-database, so the lookup-then-insert pattern is race-free.
- **No `#[auto_inc]` on non-PK columns.** Auto-inc only applies to primary keys. When the PK moved to `valid_at`, the original auto-inc ID column became caller-supplied. `players::next_player_id()` shows the scan-and-increment fallback; `cards` and `zones` expect the caller to supply ids (cards from `content/gen-ids.py`, zones via the action that creates them).

Client mirror: [pixijs/src/server/ValidAtTable.ts](../../../pixijs/src/server/ValidAtTable.ts). Same packed-key shape, same "select max `valid_at` ≤ now per id" semantics. Schema changes here ripple through bindings and require updates over there.

## The scheduled-delete sweep
Each history-style table gets its own sweep. Every `cards::write` and `players::write` ends with the corresponding `schedule_delete_*::enqueue(ctx, id, valid_at)` call:

1. Inserts a row in the private schedule table (`schedule_delete_cards` / `schedule_delete_players`) with `scheduled_at = ScheduleAt::Time(<seconds-of-valid_at>)`.
2. The `#[table(accessor = ..., scheduled(<reducer>))]` attribute wires SpacetimeDB to fire the named reducer when that timestamp is reached. Since the timestamp is "now", it fires on the next scheduler tick.
3. The reducer filters by the row's id, deletes any row whose `valid_at_time(...)` is **strictly less than** the cutoff seconds, and finally deletes its own one-shot schedule row.

The strict-less-than is what preserves the new row: the writer's `valid_at_time` equals the cutoff, so it survives. Any *later* writes have a larger `valid_at_time` and also survive.

`zones` does **not** currently have its own sweep — its `write` helper exists but no reducer enqueues a delete schedule yet. If zone-row churn becomes a problem, mirror the existing triad: a `schedule_delete_zones` table + reducer, and an `enqueue` call in `zones::write`. The two existing sweep modules are minimal templates.

## SpacetimeDB 2.x macro syntax (migration trap)
- `#[table(name = "cards", public)]` is the **1.x** form. In 2.x it produces a confusing "method `cards` not found on `Local`" cascade because the macro fails to expand. Replace with `#[table(accessor = cards, public)]`.
- `accessor = <ident>` is required; `name = "<string>"` is an optional override for the SQL table name. The accessor doubles as the default SQL name.
- Scheduled tables: `#[table(accessor = ..., scheduled(<reducer_ident>))]`. The struct must include `scheduled_at: ScheduleAt`.
- Calling another table's accessor from a different file requires the trait in scope: `use crate::cards::cards;` — the trait is named after the accessor, not the struct.
- **Non-table reducer-arg structs need `#[derive(SpacetimeType)]`.** That's how `CardStack` (in `stacks.rs`) is wired — it's a wire format, not a row in any table, but it's used as a reducer argument so SATS needs to encode/decode it.
- `#[auto_inc]` only applies to the `#[primary_key]` column. When a PK moves elsewhere, the original auto-inc column becomes caller-supplied.

## Time
- `ctx.timestamp` is a `Timestamp` (microseconds since epoch).
- `now_secs(ctx)` (defined in both `cards.rs` and `zones.rs`) does `to_micros_since_unix_epoch() / 1_000_000` and casts to `u32`. That `u32` is what goes in the low half of `valid_at`.
- To round-trip seconds back into a schedule: `ScheduleAt::Time(Timestamp::from_micros_since_unix_epoch(secs * 1_000_000))`.
- Comparing a `valid_at` (u64) to a `scheduled_at` (`ScheduleAt`) requires extracting both to seconds first — they're not directly comparable.

## Stacks (wire format)
Action reducers receive stack layouts as `CardStack` arguments — a struct, not a table:

```rust
struct CardStack {
    root: u32,                 // anchor card_id
    surface: u8, macro_zone: u32, micro_zone: u8, micro_location: u32,
    stack_up: Vec<u32>,        // root → top, index 0 sits on root
    stack_down: Vec<u32>,      // root → bottom, index 0 sits under root
}
```

`stacks::apply(ctx, &stack)` walks bottom-to-top (`stack_down.iter().rev()` → `root` → `stack_up`) and writes each card's location via `cards::update_with`:
- All cards take the stack's `surface`/`macro_zone` and the stack's q/r component of `micro_zone`.
- Per-card `stacked_state` is re-packed: `Free` for the bottom card, `OnCard` for everyone above.
- Bottom card gets `micro_location = stack.micro_location`; everyone else gets the `card_id` of the card directly below.
- Returns `Err` for empty chains, duplicates across `root`/`stack_up`/`stack_down`, or non-existent `card_id`s. Don't trust the client.

The client TypeScript will see `CardStack` as a generated binding once any public reducer takes it as an argument — until then it's only used internally.

## Pitfalls
- **`update_with` returns `None` if no row exists for the id.** All `set_*` helpers inherit this — they no-op when the row hasn't been created yet. Don't paper over with `unwrap_or_default`.
- **One-shot scheduled rows accumulate** unless the reducer deletes them. The `delete_*` reducers do this on success; if you ever return `Err` early, do the cleanup first or accept the leak.
- **Public vs private affects the wire.** Only `public` tables are subscribable from clients and appear in generated bindings. `player_sessions`, `schedule_delete_cards`, and `schedule_delete_players` are intentionally private.
- **Bindings are generated, not edited.** Wrong types on the TS side mean a wrong Rust schema; fix Rust and rerun `bin/st build`.
- **Reducer signature for scheduled tables takes the row by value.** Changing the schedule table's shape changes the reducer's arg type — keep them in sync.
- **The build runs in a container** — paths in errors are `/workspace/server/spacetimedb/...`, which maps to `spacetime/server/spacetimedb/...` on the host.
- **`gen-ids.py` runs before cargo and can fail the whole build** under `set -euo pipefail`. If `bin/st build` halts before "Compiling", the content submodule is the suspect, not your Rust changes. Cross-reference [content/AGENTS.md](../../../content/AGENTS.md) (or `git status` in `content/`) before chasing phantom Rust errors.
