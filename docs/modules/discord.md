# discord

Last verified: 2026-11-18

## Purpose
Everything that talks to Discord. Nothing outside `src/discord/` makes network calls; the rest of tritone uses the `DiscordApi` trait and `LiveState`.

## Structure
- `src/discord/types.rs`: the slim model (`Id`, `ChannelKind`, `UserRef`, `Role`, `Overwrite`, `Guild`, `Channel`, `ReadState`, `MuteUntil`, `GuildSettings`, `Message`, `ReadySnapshot`), `DiscordError`, and the REST argument types (`OutgoingMessage`, `SearchScope`, `SearchQuery`, `SearchPage`, `MuteTarget`).
- `src/discord/api.rs`: `DResult` and the `DiscordApi` trait.
- `src/discord/fake.rs` (tests only): `FakeDiscord` and `Call`.
- `src/discord/gateway/mod.rs`: `spawn` (a stub that returns at once) and the `GatewayEvent` re-export; `src/discord/gateway/events.rs`: `GatewayEvent`.
- Tests: `src/discord/types_tests.rs`, `src/discord/fake_tests.rs`.

## Public surface
- `Id(pub u64)` (`src/discord/types.rs:16`): `Copy + Eq + Hash + Ord + Default`. Serde writes a decimal string and reads a string or a number; `Display`/`FromStr`; `JsonSchema` (a digit string), so tool arguments can take it.
  - `timestamp_ms` (`src/discord/types.rs:20`): `(id >> 22) + 1420070400000`.
  - `from_timestamp_ms(ms, counter)` (`src/discord/types.rs:26`): builds an id for a time, e.g. a nonce; `counter` fills the low bits.
- `ChannelKind::from_u8` (`src/discord/types.rs:95`): 0 Text, 1 Dm, 2 Voice, 3 GroupDm, 4 Category, 5 Announcement, 10/11/12 threads, 15 Forum, 16 Media, else `Other(n)`. `is_thread()`, `is_private()` (DM or group DM).
- `UserRef::label` (`src/discord/types.rs:133`): display name, else username.
- `Guild` (`src/discord/types.rs:156`): `roles` includes @everyone, whose id is the guild id; `my_roles` are the user's roles.
- `MuteUntil::active(now_ms)` (`src/discord/types.rs:197`).
- `GuildSettings` (`src/discord/types.rs:209`): `guild_id: None` holds the DM settings; `channel_mutes` holds every channel override, including `No` (explicit unmute).
- `Message` (`src/discord/types.rs:218`): attachments are placeholders (`"image: a.png"`), embeds are titles, reactions are `(":name:" or emoji, count)`.
- `DiscordError` (`src/discord/types.rs:259`): `NeedsLogin`, `Captcha`, `Locked`, `Blocked`, `RateLimited`, `Indexing`, `Forbidden`, `NotFound`, `Api { status, code }`, `Http`. Display says what the user must do. `halts()` (`src/discord/types.rs:292`) is true for `NeedsLogin | Captcha | Locked | Blocked`: nothing retries those. `kind()` (`src/discord/types.rs:300`) is log-safe.
- `DiscordApi` (`src/discord/api.rs:13`): `history` (newest first, at most one of before/after), `around`, `recent_mentions`, `search`, `ack`, `ack_bulk` (≤100 items; callers chunk), `typing`, `send`, `react`, `create_thread`, `create_forum_post`, `leave_guild`, `set_mute`, `user`.
- `GatewayEvent` (`src/discord/gateway/events.rs:6`): `Ready`, `Resumed`, `MessageCreate`, `MessageAck`, `ChannelUpsert`, `ChannelDelete`, `GuildUpsert(guild, channels)`, `GuildDelete`, `SettingsUpdate`, `Disconnected`, `Halted`.

## FakeDiscord (`src/discord/fake.rs:37`)
- Records every call as a `Call` (`src/discord/fake.rs:17`); `calls()` returns them.
- Programmable: `history_pages` (`put_history`), `mentions`, `search_index` (filtered by scope, author, channel, min/max id, content; pages of `SEARCH_PAGE` = 25), `users`.
- `next_error` (`fail_next`): returned once by the next call, which is still recorded.
- `send` returns a `Message` with `id = nonce`, authored by `me`, records it in `sent` and adds it to the history. `create_thread` from a message takes the message's id; `create_forum_post` uses the nonce for the thread and its first message.
- `FakeDiscord::default()` posts as `Id(1)` / `U001`.

## Dependencies
serde, schemars, thiserror, async-trait.

## Gotchas & invariants
- Never log tokens, cookies or message text from here.
- `GuildUpsert` replaces the guild's whole channel set: the gateway must include the guild's joined threads in `channels`, or they're dropped.

## Testing
`cargo test discord`.
