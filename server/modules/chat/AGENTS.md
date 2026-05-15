# AGENTS.md — chat module

## Purpose
SpacetimeDB 2.1.0 server module (Rust → wasm32) for world chat. Owns `chat_messages` (public, append-only) and `chat_retention` (private, scheduled). Publishes to `resonantdust-<env>-chat`. Has no `players` / `cards` / `zones` / `souls` — gameplay state lives in [shard](../shard/AGENTS.md), an entirely separate database with its own reducer queue and wasm memory.

This module is intentionally minimal: one public reducer, two tables, ~150 lines of Rust. Most things you might think to add here probably belong in shard (or in the sidecar that will eventually mediate between the two).

## Build & iterate
- `bin/st build chat` (or `bin/st build` to build every module). Compiles via the Docker wrapper into `/workspace/server/modules/chat`; bindings land in [`pixijs/src/server/spacetime/bindings/chat/`](../../../../pixijs/src/server/spacetime/bindings/chat/).
- `bin/st publish chat` / `bin/st republish chat`. CLI loads [`spacetime.json`](spacetime.json) (anchor only — empty `{}`) plus [`spacetime.<env>.json`](spacetime.dev.json) for db name / server. `--env` is passed by `bin/st`; defaults to `dev`.
- `bin/st sql chat "SELECT * FROM chat_messages"`, `bin/st call chat send_chat_message <player_id> <name> <body>` for live introspection.
- Republishing chat **does not** affect shard state and vice versa — separate databases, separate data.

## Module map
| File | Role |
| --- | --- |
| [src/lib.rs](src/lib.rs) | three-line module list. Add new `pub mod` lines here if you grow the module. |
| [src/chat.rs](src/chat.rs) | `ChatMessage` (public, append-only) + `ChatRetention` (private, scheduled). Reducers: `init` (one-shot, seeds the retention sweep on fresh publish), `send_chat_message(sender_player_id, sender_name, body)`, `chat_retention_sweep` (fires every 60s, deletes rows older than `RETENTION_MS = 1h`). |
| [src/packed.rs](src/packed.rs) | Local copy of `pack_valid_at` / `valid_at_time` (the `[time_ms_u48 \| seq_u16]` u64 packing). The shard module has its own copy; the two evolve independently. |
| [src/sequence.rs](src/sequence.rs) | Local copy of the `sequence_counter` table + `next_sequence()` u16 allocator. Independent of shard's counter — chat sequences never collide with shard sequences because their PKs aren't compared across modules. |

## Schema
**`ChatMessage`** (public, `accessor = chat_messages`):

| Field | Type | Index | Purpose |
| --- | --- | --- | --- |
| `sent_at` | `u64` | primary key | Packed `[time_ms_u48 \| seq_u16]`. Sortable as a chronological key. |
| `sender_player_id` | `u32` | `btree` | Caller-supplied. NOT validated (no `players` table in this module). |
| `sender_name` | `String` | — | **Denormalised** — frozen at send time. Caller supplies; the module trims and rejects control chars / overlong / empty names with `ANONYMOUS_PLAYER_ID` fallback. |
| `body` | `String` | — | Trimmed, length-capped, control-char-stripped. |

**`ChatRetention`** (private, `accessor = chat_retention`, `scheduled(chat_retention_sweep)`):

Single-row recurring schedule. Seeded by `init` with `ScheduleAt::Interval(60s)`; never deleted in normal operation. If the row vanishes, the sweep stops firing and the table grows unbounded — recoverable by republishing.

## Trust model (important)
**The chat module trusts the caller.** `send_chat_message` accepts `sender_player_id` and `sender_name` as plain arguments — there's no `player_sessions` table to validate against (that table lives in [shard](../shard/AGENTS.md)), and `ctx.sender()` (the calling `Identity`) isn't cross-referenced.

Anyone who can reach the chat database's reducer endpoint can therefore spoof either field. **This is acceptable only when:**
- The clients calling `send_chat_message` are trusted (dev only), OR
- A sidecar mediates: client → sidecar → chat reducer, with the sidecar resolving `Identity → player_id → name` from the shard database before calling. The sidecar's `Identity` is the only one allowed to write to chat in production.

Until the sidecar exists, treat this as a development-only entry point. The `sender_player_id` field is kept on the row (with a btree index) so future moderation tooling can filter / repair regardless.

See [`docs/SCALING_CONCERNS.md`](../../../../docs/SCALING_CONCERNS.md) §3b for the design discussion that drove the denormalised `sender_name` choice.

## Retention
- `RETENTION_MS = 60 * 60 * 1000` (1h) and `SWEEP_INTERVAL_MS = 60_000` (60s) live as consts in [chat.rs](src/chat.rs).
- Steady-state table size: roughly `(send rate) * RETENTION_MS`. At 1 msg/player/min × 1000 players × 1h = ~60k messages.
- Sweep is currently `O(N)` over the full table per fire. [`docs/SCALING_CONCERNS.md`](../../../../docs/SCALING_CONCERNS.md) §5 has a note on switching this to a PK-bounded range scan (cheap fix when it starts to matter).

## Pitfalls
- **No `players` cross-reference.** Don't add code that reaches for `crate::players::*` — it doesn't exist here. If you need a player lookup, the caller (sidecar) provides it via reducer args.
- **`init` only runs on fresh publish.** Republishing without `--delete-data` doesn't re-run it. If `ChatRetention` ever ends up empty (manual SQL DELETE, schema migration), the sweep is dead until you reseed — easiest fix is `bin/st republish chat`, which wipes data and re-runs `init`.
- **`sender_name` is frozen.** Renaming a player (not a feature today, but if it ever lands) won't rewrite historical messages. That's by design — see the chat table doc comment and [SCALING_CONCERNS.md §3b](../../../../docs/SCALING_CONCERNS.md).
- **Bindings live under their own subdir.** `pixijs/src/server/spacetime/bindings/chat/` — separate from shard's bindings. Client connection setup needs its own `DbConnection` against the chat database with chat-specific bindings.
- **The build runs in a container** — paths in errors are `/workspace/server/modules/chat/...`, which maps to `spacetime/server/modules/chat/...` on the host.
