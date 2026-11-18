# domain

Last verified: 2026-11-18

## Purpose
The logic between the MCP tools and Discord: errors, time, the live model, the send queue and the contribution log.

## Structure
- `src/domain/errors.rs`: `Error`, `Result`.
- `src/domain/clock.rs`: `Timer`, `GapRng`, `SystemTimer`, `ThreadRng`, `now_ms`, and `fake::{FakeTimer, SeqGaps}` for tests.
- `src/domain/live.rs`: `LiveState`, `BufferPolicy`, `ConnState`, `LiveEvent`. Tests: `src/domain/live_tests.rs`, with fixtures from `src/test_support.rs`.
- `src/domain/send_queue.rs`: `SendQueue`, the paced write queue. Tests: `src/domain/send_queue_tests.rs` (a `FakeDiscord` wrapper that stamps each call with virtual time and fails chosen methods once).
- `src/domain/activity.rs`: the contribution log: `record_own`, plus `channel_label` and `message_link`.

## Public surface
- `Error` (`src/domain/errors.rs:6`): `Discord(DiscordError)`, `Ambiguous { query, candidates }`, `NotFound`, `Invalid`, `NotConnected`, `Store(anyhow::Error)`. The Display text goes to Claude and says what to do next; `kind()` (`src/domain/errors.rs:25`) is log-safe.
- `Timer` (`src/domain/clock.rs:10`): `now_ms`, `sleep_until`, `now_secs`. `now_ms(&dyn Timer)` (`src/domain/clock.rs:21`).
- `fake::FakeTimer` (`src/domain/clock.rs:63`): sleeping moves virtual time forward. `fake::SeqGaps` (`src/domain/clock.rs:87`): hands out gaps in order, then repeats the last.
- `LiveState` (`src/domain/live.rs:76`), interior `std::sync::Mutex`:
  - `new(BufferPolicy { per_channel, max_age_ms })` (`src/domain/live.rs:83`); starts `Connecting`.
  - `apply(GatewayEvent, now_ms)` (`src/domain/live.rs:119`): READY replaces the model (settings for guilds not in READY are dropped) and publishes `LiveEvent::Ready`; MESSAGE_CREATE bumps `last_message_id`, acks the channel when the user wrote it (any device), otherwise bumps `mention_count` for a DM, a direct mention, a mention of one of my roles (unless the server suppresses role mentions) or @everyone/@here (unless suppressed) (`pings_me`, `src/domain/live.rs:433`), buffers the text if the channel is buffered, and publishes `LiveEvent::Message { msg, own }`; MESSAGE_ACK sets the read state and publishes `LiveEvent::Acked`. A message in an unknown channel creates it (a DM when it has no guild). GUILD_UPDATE keeps channels and my roles; `MyRolesUpdate` replaces my roles. A channel update never moves `last_message_id` backwards.
  - `subscribe()` (`src/domain/live.rs:110`): a `tokio::sync::broadcast` receiver (capacity 1024).
  - Getters: `connection`, `me`, `guild`, `guilds`, `channel`, `channels`, `read_state` (default when unknown), `settings(Option<guild>)`, `user` (`src/domain/live.rs:230` onward). `connection()` (`src/domain/live.rs:230`) is `Halted(e)` while a halt is set, else the gateway's state.
  - Halts: `halt(e)` (`src/domain/live.rs:195`) stops every Discord call; `clear_halt()` (`src/domain/live.rs:201`) only on an explicit user action; `reject_token(token)` (`src/domain/live.rs:207`) halts with `NeedsLogin` and remembers the token's SHA-256; `try_login_with(token)` (`src/domain/live.rs:217`) refuses the rejected token (or one already tried since the halt), otherwise lifts the halt, sets `Connecting` and returns true.
  - `is_unread(channel)` (`src/domain/live.rs:272`): `last_message_id > last_acked`; a channel with messages and no read state counts as unread.
  - `discord_muted(channel, now_ms)` (`src/domain/live.rs:283`): any active mute on the guild, the channel, its category, or (for a thread) its parent and that one's category. A channel override with `muted:false` never unmutes a muted guild, as in the Discord client.
  - `can_view(channel)` (`src/domain/live.rs:305`): Discord's permission order (`channel_permissions`, `src/domain/live.rs:338`; https://docs.discord.food/topics/permissions#permission-overwrites). Owner and ADMINISTRATOR see everything; threads follow their parent; DMs are always visible; unknown channels are not.
  - `recent(channel)` (`src/domain/live.rs:320`): oldest first. `set_buffered(set)` (`src/domain/live.rs:326`): DMs and group DMs are always buffered; channels dropped from the set lose their text. The buffer prunes by count and age on insert.
  - `note_acked(channel, message)` (`src/domain/live.rs:188`): tritone acked over REST.
- `SendQueue` (`src/domain/send_queue.rs:309`): one write at a time; the first goes at once, each later one waits a random `MIN_GAP_MS..=MAX_GAP_MS` (20–60 s) after the previous write, at most `HOURLY_CAP` (30) per rolling hour (`earliest`, `src/domain/send_queue.rs:221`). Constants at `src/domain/send_queue.rs:31`.
  - `new(store, live, config, timer, rng)` (`src/domain/send_queue.rs:321`).
  - `attach(api)` (`src/domain/send_queue.rs:347`): the REST client. The write tools attach `ctx.api` on every enqueue, so a client rebuilt after a login is picked up; attaching never lifts a pause.
  - `enqueue(JobSpec)` (`src/domain/send_queue.rs:352`) → `Enqueued { job_id, position, eta_secs, note }`. `JobSpec` (`src/domain/send_queue.rs:46`): `Send { channel, content, reply_to, kind }`, `React { channel, message, emoji }`, `Thread { channel, from_message, name, content }`, `ForumPost { forum, name, content }`. Blank text, names or emoji are `Invalid`. The target label and guild are taken from `LiveState` at enqueue.
  - `status(job_id)` (`src/domain/send_queue.rs:393`) → `QueueStatus { jobs: Vec<JobView>, paused, writes_last_hour, hourly_cap }`. `JobView` (`src/domain/send_queue.rs:150`): `what`, `target`, `status`, `eta_secs`, `sent_message_id`, `thread_id`, `link`, `sent_at`, `error`, `ack_error`. The last 50 finished jobs are kept.
  - `resume()` (`src/domain/send_queue.rs:420`): the user's "continue". Refuses while `live.connection()` is `Halted(NeedsLogin)`; for any other halt calls `live.clear_halt()`; then clears the pause only when the connection is `Ready`.
  - `run()` (`src/domain/send_queue.rs:439`) loops over `step()` (`src/domain/send_queue.rs:457`), which returns `Idle` (empty, paused, no client), `NotReady` (connection not `Ready`; `run` polls every 5 s), `Waited` or `Processed`.
  - A Send, a Thread with content and a ForumPost first call `typing` and wait a random `TYPING_MIN_MS..=TYPING_MAX_MS` (2–5 s) through `Timer` (`type_for_a_while`, `src/domain/send_queue.rs:513`). Reactions skip typing but keep the gaps. A thread's first message types in the new thread.
  - The nonce is a snowflake of the send time with a 12-bit counter (`nonce_for`, `src/domain/send_queue.rs:502`), made at the first attempt and reused by the retry; forum posts pass it to `create_forum_post`.
  - After a posted message (`on_done`, `src/domain/send_queue.rs:556`): `record_own(.., via_tritone = true)`, then `ack(channel, message)` and `live.note_acked`. An ack failure sets `ack_error` and never fails the job.
  - Errors (`on_error`, `src/domain/send_queue.rs:591`): a halting error fails the job, pauses the queue with the other jobs still queued, and halts all of tritone through `live.halt(e)` unless `live` is already halted (`halt`, `src/domain/send_queue.rs:621`), which keeps a token the REST client already rejected. `RateLimited` is retried once after its Retry-After (capped at 600 s), with the same nonce, and a created thread is not created again. Anything else fails only that job, with no retry.
  - `step` pauses the queue (without failing jobs) when it finds `live` halted by something else.
- `record_own(store, live, topics, msg, guild_hint, kind, via_tritone)` (`src/domain/activity.rs:57`): logs one of the user's own server messages (DMs are skipped). The guild comes from the message, else its channel, else `guild_hint`. Insert-or-ignore; when the row already existed, `via_tritone` flips it and an explicit `kind` overrides it. Without an explicit kind the kind is `other`.
- `channel_label(live, id)` (`src/domain/activity.rs:20`): `Server › #channel`, `Server › #parent › thread`, `@user (DM)`, `a, b (group DM)`. `message_link(guild, channel, message)` (`src/domain/activity.rs:12`): `https://discord.com/channels/{guild|@me}/{channel}/{message}`.

## Dependencies
chrono, rand, sha2 (token hash), tokio (sync broadcast), the `discord` types and the `store`.

## Gotchas & invariants
- `fake` is `#[cfg(test)]`: tests only.
- `LiveEvent::Message` holds the `Message` unboxed on purpose (nearly every event is one), hence the `large_enum_variant` allow.
- `mention_count` from READY is authoritative; live increments follow the same rule as Discord's badge (direct, my roles, @everyone/@here, with the server's suppress settings).
- Queue text lives only in memory and is dropped when the job finishes; a restart drops unsent jobs. A failed job is never retried except once after a rate limit, because a retried write that had gone through would duplicate it.
- `SendQueue::resume` is the only path that calls `live.clear_halt()`, and only the `resume_queue` tool (on the user's explicit OK) calls it.
- A halt is separate from the gateway's state: Disconnected, RESUMED and READY never lift it, so a routine reconnect can't silently resume after a CAPTCHA or a block.

## Testing
`cargo test domain`.
