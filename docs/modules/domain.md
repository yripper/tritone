# domain

Last verified: 2026-11-18

## Purpose
The logic between the MCP tools and Discord: errors, time, the live model, rendering, name resolution, pending, reading, triage and the send queue.

## Structure
- `src/domain/errors.rs`: `Error`, `Result`.
- `src/domain/clock.rs`: `Timer`, `GapRng`, `SystemTimer`, `ThreadRng`, `now_ms`, and `fake::{FakeTimer, SeqGaps}` for tests.
- `src/domain/live.rs`: `LiveState`, `BufferPolicy`, `ConnState`, `LiveEvent`. Tests: `src/domain/live_tests.rs`, with fixtures from `src/test_support.rs`.
- `src/domain/render.rs`: `render_message`, `message_link`, `to_discord_mentions`. Tests: `src/domain/render_tests.rs`.
- `src/domain/resolve.rs`: `resolve_channel`, `resolve_server`, `resolve_user`, `channel_label`. Tests: `src/domain/resolve_tests.rs`.
- `src/domain/pending.rs`: `effective_watch`, `pending`, `get_pending`. Tests: `src/domain/pending_tests.rs`.
- `src/domain/reading.rs`: `read_history`, `search`, `list_servers`, `list_channels`, `channel_info`, `find_user`. Tests: `src/domain/reading_tests.rs`.
- `src/domain/triage.rs`: `mark_read`, `snooze`, `unsnooze`, `set_watch`. Tests: `src/domain/triage_tests.rs`.
- `src/domain/test_world.rs` (tests only): a connected `Ctx` over `FakeDiscord`, a READY fixture and an in-memory store.
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
- `resolve_server(query, live)` (`src/domain/resolve.rs:50`): an id, else part of the server name (exact names win); `Ambiguous` labels `Server (id)`.
- `resolve_user(query, live)` (`src/domain/resolve.rs:72`): an id (passed through), else part of a username or display name among `LiveState::users` other than me; `Ambiguous` labels `Name (@username, id)`.
- `channel_label(c, live)` (`src/domain/resolve.rs:183`): `@user (DM)`, `name (group DM)`, `Server › #channel`, `Server › #parent › thread`.
- `effective_watch(channel, live, watch, now_ms)` (`src/domain/pending.rs:56`): an explicit level on the channel, then (threads) on its parent, then on its server; else a Discord mute (`discord_muted`) is `Muted`; else DMs and group DMs are `Full`, the rest `Normal`.
- `pending(live, store, mentions, now_ms) -> Result<Vec<PendingItem>>` (`src/domain/pending.rs:127`). `PendingItem { channel, label, reason, unread_count_hint, mention_count }`, `Reason { Dm, GroupDm, Mention, Reply, FullWatch, Thread }`. A channel counts only when listable (no categories, no nameless server channels, `listable`, `src/domain/pending.rs:87`), visible, unread, and not snoozed past now. Then:
  - an explicit tritone `Muted` (channel, parent or server) hides it entirely, mentions included;
  - a Discord mute without an explicit level: pending only for a **direct mention or a reply to me found in the messages themselves** (buffered messages plus the `mentions` inbox, newer than `last_acked`), never from `mention_count`, which also counts role and @everyone pings;
  - otherwise DM → `Dm`, group DM → `GroupDm`; a direct mention → `Mention`; a reply → `Reply`; an unsuppressed role or @everyone/@here ping (`pings`, `src/domain/pending.rs:100`) or Discord's `mention_count > 0` → `Mention`; `Full` → `FullWatch`; a joined thread → `Thread`.
  - Order: DMs and group DMs, then mentions and replies, then full watch and threads; newest `last_message_id` first within each. `unread_count_hint` counts buffered unread messages (0 after a restart).
- `get_pending(ctx)` (`src/domain/pending.rs:197`): fetches the mentions inbox once (`recent_mentions(100)`, kept to 7 days; a non-halting failure falls back to read states and the buffer), runs `pending`, then for the first 15 entries (`MAX_FETCH`) fetches up to 20 unread messages with `history(after = last_acked, limit 21)` (`more_unread` when 21 come back), oldest first. **Never acks.** A DM or group DM with no read state at all is dropped when the newest message is mine (Discord's rule: no read state means unread only if the last message isn't mine). A halting error stops the call; any other per-channel error is reported on that entry.
- `read_history(ctx, channel, limit, before, peek) -> History` (`src/domain/reading.rs:82`): `{ channel_label, messages (oldest first), newest_id, more, ack_error? }`; `limit` clamped to 1..=100. Without `peek` and `before`, acks the newest message returned when it's past `last_acked`, then `live.note_acked`. A failed ack still returns the messages, with `ack_error`. `read_thread` is the same call on a thread id.
- `search(ctx, scope, query)` (`src/domain/reading.rs:116`): Discord search, rendered.
- `list_servers` (`src/domain/reading.rs:133`), `list_channels` (`src/domain/reading.rs:201`), `channel_info` (`src/domain/reading.rs:232`): listings hide categories, nameless server channels, hidden channels, and muted ones (tritone or Discord) unless the `watch` filter asks for them; DMs and group DMs first, then by label. Servers muted in tritone are left out of `list_servers` unless `include_muted`.
- `find_user(live, query)` (`src/domain/reading.rs:243`): case-insensitive substring of username or display name over `LiveState::users` (not me), whole names first, at most 25; each with its 1:1 DM and the servers where tritone saw them write (from the buffer). No member search over REST.
- `mark_read(ctx, channels)` (`src/domain/triage.rs:23`): bulk ack of each unread channel to its `last_message_id`, chunks of 100, then `note_acked`; channels with nothing unread cost no call. `snooze` (`src/domain/triage.rs:43`) refuses past times; `unsnooze`; `set_watch(ctx, target, level)` (`src/domain/triage.rs:58`) infers server or channel from `live`, else `NotFound`.
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
- `pending` reads store watch levels and snoozes but never writes. Only `read_history` (not peeking), `mark_read` and sending ack.
- The domain functions take a `Ctx` (`src/mcp/state.rs`) where they need the REST client, so tests build one over `FakeDiscord` (`test_world`).

## Testing
`cargo test domain`. Rendering and resolution: `cargo test --lib -- render resolve`. Pending, reading and triage: `cargo test --lib -- pending reading triage`.
