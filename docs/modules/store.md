# store

Last verified: 2026-11-18

## Purpose
SQLite at `~/.tritone/tritone.db` (0600, WAL). Holds triage state (watch levels, snoozes, categories), voice notes, opportunity ids and the contribution log. **Never message text.**

## Structure
- `src/store/mod.rs`: `Store` (a `Mutex<Connection>`), `open`, `open_in_memory`, and `conn()` for the store's own files.
- `src/store/schema.rs`: append-only `MIGRATIONS`, tracked with `PRAGMA user_version`.
- `src/store/kv.rs`: `kv_get`, `kv_set`.
- `src/store/watch.rs`: `WatchLevel`, `TargetKind`, watch levels and snoozes.
- `src/store/store_tests.rs`: tests.

## Schema v1 (`src/store/schema.rs:6`)
All tables are created in v1, so features add methods, not migrations:
`kv`, `watch`, `snooze`, `category`, `voice_profile`, `chat_notes`, `own_message`, `opportunity`, `activity`. Ids are stored as decimal strings.

## Public surface
- `Store::open` (`src/store/mod.rs:21`), `Store::open_in_memory` (`src/store/mod.rs:30`).
- `kv_get` / `kv_set` (`src/store/kv.rs:7`, `:11`). Known keys: `build_number`, `installation_id`.
- `WatchLevel { Full, Normal, Muted }` (`src/store/watch.rs:12`), serde snake_case and `JsonSchema`, so tools can take it as an argument.
- `TargetKind { Guild, Channel }` (`src/store/watch.rs:42`).
- `get_watch`, `set_watch` (`None` clears), `all_watch` (`src/store/watch.rs:61`, `:70`, `:84`).
- `set_snooze` (Unix ms; `None` unsnoozes), `snoozed_until` (returns the stored time even if past) (`src/store/watch.rs:93`, `:107`).

## Dependencies
rusqlite (bundled), serde, schemars. `Id` comes from `src/discord/types.rs`.

## Gotchas & invariants
- Migrations are append-only. Add a new entry and never edit a shipped one.
- The mutex recovers from poisoning, because SQLite stays consistent after a panic.
- New tables' methods go in new files under `src/store/`, using `self.conn()`.

## Testing
`cargo test store`: in-memory DB, plus one file DB to check migration idempotence and the 0600 mode.
