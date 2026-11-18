# domain

Last verified: 2026-11-18

## Purpose
The logic between the MCP tools and Discord: errors, time, the live model and the send queue.

## Structure
- `src/domain/errors.rs`: `Error`, `Result`.
- `src/domain/clock.rs`: `Timer`, `GapRng`, `SystemTimer`, `ThreadRng`, `now_ms`, and `fake::{FakeTimer, SeqGaps}` for tests.
- `src/domain/live.rs`: `LiveState`, `BufferPolicy`, `ConnState`, `LiveEvent`. Tests: `src/domain/live_tests.rs`, with fixtures from `src/test_support.rs`.
- `src/domain/render.rs`: `render_message`, `message_link`, `to_discord_mentions`. Tests: `src/domain/render_tests.rs`.
- `src/domain/resolve.rs`: `resolve_channel`. Tests: `src/domain/resolve_tests.rs`.
- `src/domain/send_queue.rs`: `SendQueue`, a stub with the final public signatures.

## Public surface
- `Error` (`src/domain/errors.rs:6`): `Discord(DiscordError)`, `Ambiguous { query, candidates }`, `NotFound`, `Invalid`, `NotConnected`, `Store(anyhow::Error)`. The Display text goes to Claude and says what to do next; `kind()` (`src/domain/errors.rs:25`) is log-safe.
- `Timer` (`src/domain/clock.rs:10`): `now_ms`, `sleep_until`, `now_secs`. `now_ms(&dyn Timer)` (`src/domain/clock.rs:21`).
- `fake::FakeTimer` (`src/domain/clock.rs:63`): sleeping moves virtual time forward. `fake::SeqGaps` (`src/domain/clock.rs:87`): hands out gaps in order, then repeats the last.
- `LiveState` (`src/domain/live.rs:76`), interior `std::sync::Mutex`:
  - `new(BufferPolicy { per_channel, max_age_ms })` (`src/domain/live.rs:83`); starts `Connecting`.
  - `apply(GatewayEvent, now_ms)` (`src/domain/live.rs:119`): READY replaces the model (settings for guilds not in READY are dropped) and publishes `LiveEvent::Ready`; MESSAGE_CREATE bumps `last_message_id`, acks the channel when the user wrote it (any device), otherwise bumps `mention_count` for a DM, a direct mention, a mention of one of my roles (unless the server suppresses role mentions) or @everyone/@here (unless suppressed) (`pings_me`, `src/domain/live.rs:433`), buffers the text if the channel is buffered, and publishes `LiveEvent::Message { msg, own }`; MESSAGE_ACK sets the read state and publishes `LiveEvent::Acked`. A message in an unknown channel creates it (a DM when it has no guild). GUILD_UPDATE keeps channels and my roles; `MyRolesUpdate` replaces my roles. A channel update never moves `last_message_id` backwards.
  - `subscribe()` (`src/domain/live.rs:110`): a `tokio::sync::broadcast` receiver (capacity 1024).
  - Getters: `connection`, `me`, `guild`, `guilds`, `channel`, `channels`, `read_state` (default when unknown), `settings(Option<guild>)`, `user` (`src/domain/live.rs:230` onward). `users()` (`src/domain/live.rs:271`): every known user (READY users, DM recipients, authors seen) plus me. `connection()` (`src/domain/live.rs:230`) is `Halted(e)` while a halt is set, else the gateway's state.
  - Halts: `halt(e)` (`src/domain/live.rs:195`) stops every Discord call; `clear_halt()` (`src/domain/live.rs:201`) only on an explicit user action; `reject_token(token)` (`src/domain/live.rs:207`) halts with `NeedsLogin` and remembers the token's SHA-256; `try_login_with(token)` (`src/domain/live.rs:217`) refuses the rejected token (or one already tried since the halt), otherwise lifts the halt, sets `Connecting` and returns true.
  - `is_unread(channel)` (`src/domain/live.rs:272`): `last_message_id > last_acked`; a channel with messages and no read state counts as unread.
  - `discord_muted(channel, now_ms)` (`src/domain/live.rs:283`): any active mute on the guild, the channel, its category, or (for a thread) its parent and that one's category. A channel override with `muted:false` never unmutes a muted guild, as in the Discord client.
  - `can_view(channel)` (`src/domain/live.rs:305`): Discord's permission order (`channel_permissions`, `src/domain/live.rs:338`; https://docs.discord.food/topics/permissions#permission-overwrites). Owner and ADMINISTRATOR see everything; threads follow their parent; DMs are always visible; unknown channels are not.
  - `recent(channel)` (`src/domain/live.rs:320`): oldest first. `set_buffered(set)` (`src/domain/live.rs:326`): DMs and group DMs are always buffered; channels dropped from the set lose their text. The buffer prunes by count and age on insert.
  - `note_acked(channel, message)` (`src/domain/live.rs:188`): tritone acked over REST.
- `render_message(m, live) -> RenderedMessage` (`src/domain/render.rs:41`): `{ id, from (display name else username), from_id, at (RFC 3339 UTC), text, reply_to: Option<ReplyRef { message_id, author }>, reactions, link }`. In `text`, outside code (`map_outside_code`, `src/domain/render.rs:82`):
  - `<@id>`/`<@!id>` → `@label` or `@unknown-user`; `<#id>` → `#name` or `#unknown-channel`; custom emoji `<:name:id>`/`<a:name:id>` → `:name:` (`token_label`, `src/domain/render.rs:140`);
  - `<@&id>` → `@everyone` for the guild's own id, `@role:<id>` for another role of the guild, else `@unknown-role` (`role_label`, `src/domain/render.rs:161`): roles carry no name in the model;
  - attachments (`[image: …]`, `[file: …]`) and embeds (`[embed: title]`) are appended one per line;
  - a reply whose author Discord didn't send (deleted message) has `author: "unknown-user"`.
  Syntax reference: https://docs.discord.food/reference#message-formatting
- `message_link(guild, channel, message)` (`src/domain/render.rs:71`): `https://discord.com/channels/{guild|@me}/{channel}/{message}`.
- `to_discord_mentions(text, live, guild)` (`src/domain/render.rs:176`): `@name` → `<@id>` when exactly one person matches the username or display name, case-insensitively. In a server any known user (`LiveState::users`, no member lists exist) counts; with `guild: None`, only DM and group DM people. `@everyone`, `@here`, e-mail addresses, code and ambiguous names stay as written; a trailing `.` isn't part of the name.
- `resolve_channel(query, live) -> Result<Id>` (`src/domain/resolve.rs:19`), in order: a bare id; a `discord.com/channels/{g|@me}/{c}[/{m}]` link (any subdomain); `@user` (1:1 DMs by username or display name); `server/#name`; `#name`; else a bare substring over server channels, 1:1 DMs and group DM names. Case-insensitive substring matching, but exact names win over partial ones (`#general` never matches `#general-help` when a `#general` exists). Server channels need `can_view`, a name (nameless phantom channels are skipped) and must not be categories. A known id or link to a hidden channel is `NotFound`; an unknown id passes through for Discord to answer. Several matches → `Error::Ambiguous` with up to 10 sorted labels `Server › #channel (id)`, `@user (DM, id)` or `name (group DM, id)`.
- `SendQueue::new(store, timer, rng)`, `attach`, `run`, `enqueue` (`Err(Invalid("not implemented"))`), `status` (`src/domain/send_queue.rs:22` onward).

## Dependencies
chrono, rand, sha2 (token hash), tokio (sync broadcast), the `discord` types and the `store`.

## Gotchas & invariants
- `fake` is `#[cfg(test)]`: tests only.
- `LiveEvent::Message` holds the `Message` unboxed on purpose (nearly every event is one), hence the `large_enum_variant` allow.
- `mention_count` from READY is authoritative; live increments follow the same rule as Discord's badge (direct, my roles, @everyone/@here, with the server's suppress settings).
- A halt is separate from the gateway's state: Disconnected, RESUMED and READY never lift it, so a routine reconnect can't silently resume after a CAPTCHA or a block.

- `render` leaves fenced and inline code verbatim, like the Discord client; an unclosed fence is plain text.
- Ambiguous labels carry the id, because the error tells Claude to pass the id.

## Testing
`cargo test domain`. Rendering and resolution: `cargo test --lib -- render resolve`.
