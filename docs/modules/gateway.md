# gateway

Last verified: 2026-11-19

## Purpose

The one persistent gateway connection, like Discord web's. It connects, identifies or resumes, heartbeats, slims READY, and turns dispatches into `GatewayEvent`s for `LiveState::apply`. It also builds the REST client (`AppState::set_api`) and rebuilds it when a new login arrives.

## Structure

- `src/discord/gateway/mod.rs`: `spawn(state)`. It owns the daemon's one `CookieJar` (and its saver) and the `Fingerprint`, refreshed once at start. `HttpOpener` wraps `discord::http::rebuild_api`.
- `src/discord/gateway/conn.rs`: the connection loop (`Gateway`), plus `Timing`, `Backoff`, `identify_payload` and the resume URL check.
- `src/discord/gateway/zlib.rs`: `zlib-stream` inflater, one per connection.
- `src/discord/gateway/ready.rs`: `parse_ready`, `supplemental_roles`.
- `src/discord/gateway/events.rs`: `GatewayEvent` and `parse_dispatch`.
- `src/discord/gateway/objects.rs`: JSON → guild, role, settings, and channel with gateway context. It builds on `discord::parse`.
- `src/discord/gateway/subscribe.rs`: miss detection, op 37, full-watch channels.

## Public surface

- `spawn(state: AppState) -> JoinHandle<()>` (`src/discord/gateway/mod.rs:45`): runs for the daemon's lifetime. Called from `cli::daemon::start`.
- `GatewayEvent` (`src/discord/gateway/events.rs`), re-exported as `crate::discord::gateway::GatewayEvent`. This module adds `RoleUpsert { guild, role }` (`src/discord/gateway/events.rs:39`) and `RoleDelete { guild, role }` (`src/discord/gateway/events.rs:44`) for GUILD_ROLE_*; `LiveState` applies them (`src/domain/live.rs:170`).

## Key methods

- `Gateway::run` (`src/discord/gateway/conn.rs:228`): token → `Opener::open` → `set_api` → `session_loop`. On a halt it waits for a login; on a new token it opens a new session at once.
- `wait_for_token` (`src/discord/gateway/conn.rs:257`): with no keyring token it calls `live.halt(NeedsLogin)` and waits on `login_notify`.
- `wait_for_login` (`src/discord/gateway/conn.rs:272`): ignores a wake-up while the live state is still halted, so a stale notify can't drive an identify loop.
- `session_loop` (`src/discord/gateway/conn.rs:283`): reconnects with one token.
  - Backoff is `min(30 s, 1 s << n)` plus 0–1 s of jitter, reset after a connection that stayed ready for 60 s (`Timing::default`, `src/discord/gateway/conn.rs:67`).
  - After 3 resumes that never reach RESUMED, it identifies fresh.
  - While waiting it still listens for a new login.
- `connect_once` (`src/discord/gateway/conn.rs:357`):
  - `ws_connect` to `<base>/?v=9&encoding=json&compress=zlib-stream`, with `fp.ws_headers()`. The connect timeout is 15 s and HELLO's is 10 s.
  - Then op 2, or op 6 `{token, session_id, seq}` to the resume URL.
- `on_close` (`src/discord/gateway/conn.rs:405`):
  - 4004 → `live.reject_token(token)` and halt;
  - 4001–4003, 4005, 4008 and 4010–4016 → `live.halt(GatewayClosed { code })` and halt. `status` then says "Discord closed the session (code N): restart tritone (pm2 restart tritone)";
  - 4007 or 4009 → identify after 1–5 s;
  - anything else, or no close frame → resume.
  
  Source: https://docs.discord.food/gateway/opcodes-and-close-codes#gateway-close-event-codes
- `Live::pump` (`src/discord/gateway/conn.rs:507`): one socket's loop.
  - The first heartbeat goes at a random point of the interval. A beat still unacked at the next one means a zombie: close 4000 and resume.
  - A server op 1 is answered at once.
  - A login notify with a different keyring token closes 4000 and identifies fresh with it (reviewer condition C2).
- `on_payload` (`src/discord/gateway/conn.rs:549`): op 7 → resume; op 9 `true` → resume, `false` → identify after 1–5 s. https://docs.discord.food/gateway/using-gateway#resuming
- `on_ready` (`src/discord/gateway/conn.rs:613`):
  - `parse_ready`, then the session (the resume URL only if `resume_url_allowed`);
  - op 37 for missed guilds (only on a fresh READY after a drop);
  - `apply(Ready)`, then `live.set_buffered(full-watch channels)`.
- `identify_payload` (`src/discord/gateway/conn.rs:158`):
  - capabilities 253;
  - `properties` = `fp.identify_properties()`;
  - presence `{status:"unknown", since:0, activities:[], afk:true}`;
  - `compress:false`;
  - cold-start `client_state`;
  - no `intents`.
  
  Source: https://docs.discord.food/gateway/gateway-events#identify
- `Inflater::push` (`src/discord/gateway/zlib.rs`): buffers frames until they end in `00 00 ff ff`, then inflates with a sync flush. It caps a message at `WS_MAX_SIZE` (256 MiB). https://docs.discord.food/gateway/using-gateway#transport-compression
- `parse_ready(ready, supplemental)` (`src/discord/gateway/ready.rs:21`):
  - `read_state` comes versioned (`{entries,…}`) or bare. Only `read_state_type` 0 (or missing) is kept, and `last_message_id` 0 counts as nothing acked.
  - DMs: `recipient_ids` are resolved against `users` (and `relationships[].user`).
  - `my_roles` come only from the user's own entry in `merged_members` (index-aligned with `guilds`), else from the supplemental's own entry. Other users' entries are never used: with PRIORITIZED_READY_PAYLOAD, READY_SUPPLEMENTAL's `merged_members` hold other people's members (https://docs.discord.food/gateway/gateway-events#ready, note 7).
  - `user_guild_settings` for guilds not in READY are dropped.
  - Guilds can be plain or carry `properties`.
- `supplemental_roles` (`src/discord/gateway/ready.rs:104`): `MyRolesUpdate` only for guilds whose READY_SUPPLEMENTAL list holds the user's own member (normally none: READY carries it).
- `parse_dispatch(t, d, me)` (`src/discord/gateway/events.rs:61`) → `Vec<GatewayEvent>`:
  - MESSAGE_CREATE and MESSAGE_ACK;
  - CHANNEL_CREATE/UPDATE and THREAD_CREATE/UPDATE → `ChannelUpsert`; CHANNEL_DELETE and THREAD_DELETE → `ChannelDelete`;
  - THREAD_LIST_SYNC → one `ChannelUpsert` per thread;
  - GUILD_CREATE → `GuildUpsert`, with channels plus threads, and my roles from `members`;
  - GUILD_UPDATE, and GUILD_DELETE (skipped when `unavailable`, an outage);
  - GUILD_MEMBER_UPDATE for `me` → `MyRolesUpdate`;
  - GUILD_ROLE_CREATE/UPDATE/DELETE;
  - USER_GUILD_SETTINGS_UPDATE.
  
  Everything else (SESSIONS_REPLACE, PRESENCE_UPDATE, …) gives nothing, without a log line.
- `missed_guilds(snapshot, seen, cutoff)` (`src/discord/gateway/subscribe.rs:29`): guilds with a channel whose READY `last_message_id` is past the highest id seen live yet older than the drop (`cutoff`). Messages from the downtime are expected gaps; DMs and channels with no baseline are skipped.
- `subscription_packet(guild, channels)` (`src/discord/gateway/subscribe.rs:48`): op 37 `{typing, threads, activities: true, member_updates: false, members: [], channels: {id: [[0,99]]}, thread_member_lists: []}`.
- `subscribe_channels(channels, guild, watched)` (`src/discord/gateway/subscribe.rs:67`): the channels in a guild's op 37. Only watched text, announcement, forum and media channels qualify, most recently active first, at most `MAX_SUBSCRIBED_CHANNELS` (5). That keeps the payload far under the 15 KiB limit (over it, Discord closes with 4002, a fatal code) and keeps the member-list ranges few.
- `full_watch_channels` (`src/discord/gateway/subscribe.rs:81`): the channel's own level, else (for a thread) its parent's, else its guild's.

## Dependencies

- `discord::transport`: `ws_connect`, `http_client`, `CookieJar`, `WS_MAX_SIZE`.
- `discord::fingerprint`: `identify_properties`, `ws_headers`, `refresh`.
- `discord::http::rebuild_api`.
- `discord::parse`: `parse_message`, `parse_user`, `parse_channel`.
- `domain::live::LiveState`, `mcp::state::AppState` (`login_notify`, `set_api`), `store` (`all_watch`), `secrets` (`token`).
- Crates: tokio-tungstenite 0.30 (frames), flate2 (zlib), futures-util, rand.

## Gotchas & invariants

- **Never close with 1000 or 1001:** that ends the session and shows the user offline. Every close tritone sends is 4000. Dropping the process just drops TCP.
- **The dead token is never retried.** 4004 goes through `reject_token`, and `status` only wakes the gateway for a different token (`try_login_with`). A REST 401 (`HttpDiscord`) can come first; the gateway keeps its socket until a new login arrives, then closes 4000 and identifies with the new token.
- **A fatal close code halts with `DiscordError::GatewayClosed { code }`, and only a restart recovers from it.** On any halt the loop also applies `Disconnected`, so the model never looks ready with a dead socket. Even if something lifts the halt (`SendQueue::resume` lifts any non-login halt), `connection()` stays `Reconnecting`, not `Ready`.
- **wreq's 30 s client timeout doesn't reach the upgraded socket:** it wraps the request and its body only. `socket_outlives_the_http_client_timeout` holds a socket for 3 paused minutes.
- **The resume URL comes from READY,** but the token only goes to `*.discord.gg` over `wss` (or the host tritone was pointed at). Otherwise it resumes through the main gateway.
- **Op 37's `channels: {id: [[0,99]]}` asks for the first 100 member-list rows of those channels.** That is the request the web client sends when you open a channel, so it's kept on purpose. The spec's "never subscribe to member lists" means no member-list *scraping*. To keep it that way, op 37 goes only to guilds where the READY diff shows missed messages (never at startup), and to at most 5 watched text-like channels per guild.
- **The miss check runs only on a fresh READY after a drop.** `cutoff` is the first drop since the last READY/RESUMED; RESUMED clears it, because a resume replays what was missed.
- **Broadcast consumers can lag.** `LiveState::subscribe` is a tokio broadcast channel (capacity 1024). Anyone who subscribes must handle `RecvError::Lagged(n)` (count the skipped events and resync from `LiveState`), or it will drop events or stop on the error.
- **`set_buffered` is set on every READY only.** A watch-level change made later has to call `live.set_buffered` itself.
- **No message text, token or cookie is logged.** Logs carry close codes, opcodes, counts and the `resume`/`resumable` flags.

## Testing

- **Unit tests:**
  - `zlib_tests.rs`: a multi-frame stream, a split frame, corrupt data, the cap.
  - `ready_tests.rs`: versioned and bare read states, the settings filter, `recipient_ids`, merged members from READY or supplemental.
  - `events_tests.rs`: every handled dispatch, plus the ignored ones.
  - `subscribe_tests.rs`.
- **`conn_tests.rs`:** a scripted local tokio-tungstenite server speaking zlib-stream, with a `FakeOpener`. It runs in real time with `Timing` scaled to milliseconds: paused time can't drive the loop, because tokio auto-advances it while loopback IO is in flight and fires the connect timeouts. Only the long-socket test pauses time, once the socket is up.
- Run: `cargo test gateway`.
