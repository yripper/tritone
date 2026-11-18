# store

Last verified: 2026-11-19

## Purpose
SQLite at `~/.tritone/tritone.db` (0600, WAL). Holds triage state (watch levels, snoozes, categories), voice notes, opportunity ids and the contribution log. **Never message text.**

## Structure
- `src/store/mod.rs`: `Store` (a `Mutex<Connection>`), `open`, `open_in_memory`, and `conn()` for the store's own files.
- `src/store/schema.rs`: append-only `MIGRATIONS`, tracked with `PRAGMA user_version`.
- `src/store/kv.rs`: `kv_get`, `kv_set`.
- `src/store/watch.rs`: `WatchLevel`, `TargetKind`, watch levels and snoozes.
- `src/store/activity.rs`: the contribution log (`ActivityRow`). Tests: `src/store/activity_tests.rs`.
- `src/store/voice.rs`: voice profiles, channel notes, categories and `own_message` (`VoiceEntry`, `OwnMessageRow`). Tests: `src/store/voice_tests.rs`.
- `src/store/store_tests.rs`: tests.

## Schema v1 (`src/store/schema.rs:6`)
All tables are created in v1, so features add methods, not migrations:
`kv`, `watch`, `snooze`, `category`, `voice_profile`, `chat_notes`, `own_message`, `opportunity`, `activity`. Ids are stored as decimal strings.

## Public surface
- `Store::open` (`src/store/mod.rs:21`), `Store::open_in_memory` (`src/store/mod.rs:30`).
- `kv_get` / `kv_set` (`src/store/kv.rs:7`, `:11`). Known keys: `build_number`, `installation_id`, `backfill:<guild>` (backfill progress JSON), `backfill_last_error` (why the last backfill run stopped; empty when cleared).
- `WatchLevel { Full, Normal, Muted }` (`src/store/watch.rs:12`), serde snake_case and `JsonSchema`, so tools can take it as an argument.
- `TargetKind { Guild, Channel }` (`src/store/watch.rs:42`).
- `get_watch`, `set_watch` (`None` clears), `all_watch` (`src/store/watch.rs:61`, `:70`, `:84`).
- `set_snooze` (Unix ms; `None` unsnoozes), `snoozed_until` (returns the stored time even if past) (`src/store/watch.rs:93`, `:107`).
- `ActivityRow { message_id, guild_id, channel_id, created_at, kind, is_reply, via_tritone }` (`src/store/activity.rs:11`).
- `record_activity(&row) -> bool` (`src/store/activity.rs:43`): insert-or-ignore, true when inserted. `mark_sent_via_tritone(id) -> bool` (`src/store/activity.rs:63`): false when there's no row yet. `set_activity_kind(id, kind)` (`src/store/activity.rs:72`). `activity_between(from_ms, to_ms)` (`src/store/activity.rs:79`): `from <= created_at < to`, newest first.

- Voice (`src/store/voice.rs`), times in Unix ms:
  - `profile(category)`, `save_profile(category, body, now_ms)` (replaces), `profile_dates()` (no bodies) (`src/store/voice.rs:49`, `:53`, `:62`); `notes(channel)`, `save_notes(channel, body, now_ms)` (`:69`, `:73`).
  - `category(target)`, `set_category(target, category)`, `targets_in_category(category)` (`src/store/voice.rs:89`, `:96`, `:105`). `target_id` is a channel or a server id.
  - `record_own_messages(rows, now_ms)` (`src/store/voice.rs:114`): insert-or-ignore, then prunes rows older than `OWN_MESSAGE_KEEP_MS` (22 days).
  - `own_count_in_channel(channel, since_ms)` and `own_since(since_ms)` (`src/store/voice.rs:135`, `:145`): strictly after `since_ms`, leaving out messages the contribution log marks `via_tritone`.
  - `recent_own_channels(limit)` (`src/store/voice.rs:156`): `(channel, guild)` newest first, from `own_message` and `activity` together. `sent_via_tritone(ids)` (`src/store/voice.rs:169`).

## Dependencies
rusqlite (bundled), serde, schemars. `Id` comes from `src/discord/types.rs`.

## Gotchas & invariants
- Migrations are append-only. Add a new entry and never edit a shipped one.
- The mutex recovers from poisoning, because SQLite stays consistent after a panic.
- New tables' methods go in new files under `src/store/`, using `self.conn()`.
- `category` is keyed by `target_id` alone. A server whose default channel shares its id has one category for both (the channel would inherit the server's anyway).

## Testing
`cargo test store`: in-memory DB, plus one file DB to check migration idempotence and the 0600 mode.
