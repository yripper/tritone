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
- `src/store/opportunity.rs`: opportunity rows (`OpportunityRow`, `OpportunityStatus`). Tests: `src/store/opportunity_tests.rs`.
- `src/store/voice.rs`: voice profiles, channel notes, categories and `own_message` (`VoiceEntry`, `OwnMessageRow`). Tests: `src/store/voice_tests.rs`.
- `src/store/store_tests.rs`: tests.

## Schema (`src/store/schema.rs:6`)
- **v1** creates every table: `kv`, `watch`, `snooze`, `category`, `voice_profile`, `chat_notes`, `own_message`, `opportunity`, `activity`. Ids are stored as decimal strings.
- **v2** (`src/store/schema.rs:58`) rebuilds `watch` keyed by `(target_id, kind)` and copies the rows: an older server's default channel has the server's own id, so a server level and a channel level can share an id.

## Public surface
- `Store::open` (`src/store/mod.rs:21`), `Store::open_in_memory` (`src/store/mod.rs:30`).
- `kv_get` / `kv_set` (`src/store/kv.rs:7`, `:11`). Known keys: `build_number`, `installation_id`, `cleanup_last_post:<guild>` (cleanup's searched last post, JSON `{checked_at, last}`, trusted 24 h), `backfill:<guild>` (backfill progress JSON), `backfill_last_error` (why the last backfill run stopped; empty when cleared).
- `WatchLevel { Full, Normal, Muted }` (`src/store/watch.rs:12`), serde snake_case and `JsonSchema`, so tools can take it as an argument.
- `TargetKind { Guild, Channel }` (`src/store/watch.rs:43`), part of the watch key.
- `WatchMap { guilds, channels }` (`src/store/watch.rs:59`): every explicit level, split by kind.
- `get_watch(target, kind)`, `set_watch(target, kind, level)` (`None` clears that kind only), `all_watch() -> WatchMap` (`src/store/watch.rs:69`, `:82`, `:98`).
- `set_snooze` (Unix ms; `None` unsnoozes), `snoozed_until` (returns the stored time even if past) (`src/store/watch.rs:116`, `:130`).
- `ActivityRow { message_id, guild_id, channel_id, created_at, kind, is_reply, via_tritone }` (`src/store/activity.rs:11`).
- `record_activity(&row) -> bool` (`src/store/activity.rs:43`): insert-or-ignore, true when inserted. `mark_sent_via_tritone(id) -> bool` (`src/store/activity.rs:63`): false when there's no row yet. `set_activity_kind(id, kind)` (`src/store/activity.rs:72`). `activity_between(from_ms, to_ms)` (`src/store/activity.rs:79`): `from <= created_at < to`, newest first.

- Opportunities (`src/store/opportunity.rs`): `OpportunityStatus { Open, Dismissed, Answered, Gone }` (`:12`), `OpportunityRow { message_id, channel_id, guild_id, rule, created_at, status, updated_at }` (`:43`).
  - `add_opportunity(&row) -> bool` (`src/store/opportunity.rs:78`): insert-or-ignore, so the first rule wins and a dismissed one never reopens. `opportunity(id)` (`:94`). `open_opportunities(since_ms)` (`:106`): newest first.
  - `set_opportunity_status(id, status, now_ms) -> bool` (`src/store/opportunity.rs:117`); `opportunity_answered(id, now_ms) -> bool` (`:127`) changes only an open row.
  - `wrote_in(channel)` and `is_own_message(id)` (`src/store/opportunity.rs:136`, `:146`): from `own_message` and `activity` together.
- Voice (`src/store/voice.rs`), times in Unix ms:
  - `profile(category)`, `save_profile(category, body, now_ms)` (replaces), `profile_dates()` (no bodies) (`src/store/voice.rs:49`, `:53`, `:62`); `notes(channel)`, `save_notes(channel, body, now_ms)` (`:69`, `:73`).
  - `category(target)`, `set_category(target, category)`, `targets_in_category(category)` (`src/store/voice.rs:89`, `:96`, `:105`). `target_id` is a channel or a server id.
  - `record_own_messages(rows, now_ms)` (`src/store/voice.rs:114`): insert-or-ignore, then prunes rows older than `OWN_MESSAGE_KEEP_MS` (22 days).
  - `own_count_in_channel(channel, since_ms)` and `own_since(since_ms)` (`src/store/voice.rs:135`, `:145`): strictly after `since_ms`, leaving out messages the contribution log marks `via_tritone`.
  - `recent_own_channels(limit)` (`src/store/voice.rs:156`): `(channel, guild)` newest first, from `own_message` and `activity` together. `sent_via_tritone(ids)` (`src/store/voice.rs:193`).
  - `last_own_post_in_guild(guild)` and `own_channels_in_guild(guild)` (`src/store/voice.rs:170`, `:182`): from `own_message` and `activity` together, for cleanup.

## Dependencies
rusqlite (bundled), serde, schemars. `Id` comes from `src/discord/types.rs`.

## Gotchas & invariants
- Migrations are append-only. Add a new entry and never edit a shipped one.
- The mutex recovers from poisoning, because SQLite stays consistent after a panic.
- New tables' methods go in new files under `src/store/`, using `self.conn()`.
- `category` is keyed by `target_id` alone. A server whose default channel shares its id has one category for both (the channel would inherit the server's anyway).

## Testing
`cargo test store`: in-memory DB, plus one file DB to check migration idempotence and the 0600 mode; `migration_v2_keeps_rows` runs v1, inserts rows, then migrates; `guild_and_default_channel_same_id_both_watchable`.
