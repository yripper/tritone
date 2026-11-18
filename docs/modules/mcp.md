# mcp

Last verified: 2026-11-19

## Purpose
The MCP server Claude talks to: Streamable HTTP on `127.0.0.1:<port>/mcp` (default 7781). Stateless, behind a Host check and a bearer token. Tools are split into one router per group.

## Structure
- `src/mcp/server.rs`: `TritoneServer`, `tool_router()` (merges the group routers with `+`), `run`, `json`, and the `ServerHandler` impl.
- `src/mcp/tools/`: one `#[tool_router(router = <group>_router, vis = "pub(crate)")] impl TritoneServer` per file: `status.rs`, `read.rs`, `triage.rs`, `write.rs`, `engage.rs`, `activity.rs`. Tests for the read and triage tools: `src/mcp/tools/read_tests.rs`.
- `src/mcp/state.rs`: `AppState`, `Ctx`.
- `src/mcp/http.rs`: `guard_router` (Host + bearer), `app` (the guarded `/mcp` service) and `serve` (graceful shutdown on SIGTERM/SIGINT).
- `src/mcp/instructions.rs`: `BASE_INSTRUCTIONS`, `build_instructions`, `read_style_guide`, `current_instructions`.
- Tests: `src/mcp/mcp_tests.rs`, `src/mcp/http_tests.rs`.

## Tools
- `status` (`src/mcp/tools/status.rs:26`): `connection` (`connecting`, `ready`, `reconnecting`, `needs_login`, `halted`), `user` (username only), `guilds` (a count), `note`. While halted on `NeedsLogin`, a keyring token that `LiveState::try_login_with` accepts (not the rejected one, nor one already tried) wakes the gateway through `login_notify` and reports `connecting`; the dead token never does, so polling `status` can't loop identifies (`src/mcp/tools/status.rs:45`).
- **Read** (`src/mcp/tools/read.rs`). Channel arguments go through `resolve_channel` (id, link, `#name`, `server/#name`, `@user`, part of a name); an ambiguous name is a tool error listing the candidates with ids, and nothing is read.
  - `list_servers{include_muted?}` (`src/mcp/tools/read.rs:101`) → `{servers}`.
  - `list_channels{kind? (dm|group_dm|server|thread), server?, watch?}` (`src/mcp/tools/read.rs:114`) → `{channels, truncated}`, at most 300.
  - `get_pending` (`src/mcp/tools/read.rs:130`) → `{pending: [{channel, label, reason, unread_count_hint, mention_count, messages, more_unread, pings?, error?}]}`. Never acks.
  - `read_history{channel, limit=50, before?, peek=false}` (`src/mcp/tools/read.rs:137`): marks read unless `peek` or `before`.
  - `read_thread{thread, limit=50, peek=false}` (`src/mcp/tools/read.rs:148`).
  - `search_messages{query, server?, channel?, author?, offset?}` (`src/mcp/tools/read.rs:160`): a server channel is searched through its server with `channel_id`; a DM through the channel endpoint; neither is an error. `author` goes through `resolve_user`. Offset capped at 9,975 (https://docs.discord.food/resources/message#search-guild-messages).
  - `get_channel_info{channel}` (`src/mcp/tools/read.rs:194`), `find_user{query}` (`src/mcp/tools/read.rs:206`) → `{users}`.
- **Triage** (`src/mcp/tools/triage.rs`):
  - `mark_read{channels[]}` (`src/mcp/tools/triage.rs:85`): every name must resolve first, or nothing is acked; → `{marked, already_read, not_found}`.
  - `snooze{channel, until}` (`src/mcp/tools/triage.rs:95`): `until` is RFC 3339 with offset; past times are refused. `unsnooze{channel}` (`src/mcp/tools/triage.rs:106`).
  - `set_watch_level{target, level: full|normal|muted|clear}` (`src/mcp/tools/triage.rs:119`): channel-shaped targets (`#`, `@`, `/#`, links) are channels; a bare id is whichever exists (one that is both a server and its default channel must be named instead); otherwise a server name, then a channel name (`resolve_target`, `src/mcp/tools/triage.rs:56`). → `{target, kind: server|channel, level}`.
- Write tools (`src/mcp/tools/write.rs`). None writes inside the call: each validates, then enqueues on the `SendQueue` with the current REST client and returns `{ job_id, position, eta_secs, note }`.
  - `send_message{channel, text, reply_to?, kind?}` (`src/mcp/tools/write.rs:107`), `react{channel, message, emoji}` (`:121`), `create_thread{channel, from_message?, name, text?}` (`:133`, text or announcement channels only), `create_forum_post{forum, title, text}` (`:151`, forum or media channels only).
  - `channel`/`forum` is an id or a `discord.com/channels/...` link of a channel `LiveState` knows (`target`, `src/mcp/tools/write.rs:65`). Outgoing text goes through `outgoing_text` (`src/mcp/tools/write.rs:82`), the one place `@username` → `<@id>` conversion plugs in.
  - `send_status{job_id?}` (`src/mcp/tools/write.rs:169`) and `resume_queue` (`src/mcp/tools/write.rs:180`) don't go through `run`, so they work while halted. `resume_queue` first attaches the REST client `AppState` holds now, then calls `SendQueue::resume`: refuses on `NeedsLogin`, otherwise lifts `live`'s and the client's halt and resumes the queue (after a block, writes still wait its Retry-After).
- Activity tools (`src/mcp/tools/activity.rs`):
  - `my_activity{days?}` (`src/mcp/tools/activity.rs:50`): the last `days` (default 30, at least 1) through `domain::activity::my_activity`. Reads only the store and `LiveState`, so it works while halted.
  - `backfill_activity` (`src/mcp/tools/activity.rs:62`): refuses until the connection is `Ready` (before READY no guilds are known). Unless every guild is done, starts `domain::activity::backfill` in a background task, at most one at a time (`BACKFILL_RUNNING`, `src/mcp/tools/activity.rs:22`); returns `{ running, guilds_done, guilds_total, recorded, messages_seen, last_error?, note }`. A run that stops stores its error in `kv` `backfill_last_error`; the next call continues it.
- The engage group is empty so far.

## Key methods
- `TritoneServer::tool_router` (`src/mcp/server.rs:38`): `status_router + read_router + triage_router + write_router + engage_router + activity_router`. Built once in `new` and kept in the struct.
- `TritoneServer::run` (`src/mcp/server.rs:49`): gets a `Ctx` from `AppState::ctx` and runs the action. Results are pretty JSON (`json`, `src/mcp/server.rs:25`). Errors come back as **tool errors** (`is_error: true`) with text Claude can act on; only the error kind is logged.
- `AppState::ctx` (`src/mcp/state.rs:74`): a halted connection returns its `DiscordError` (e.g. `NeedsLogin`, "run `tritone login`"); no REST client yet returns `NotConnected`.
- `AppState::set_api` (`src/mcp/state.rs:67`): the gateway stores the REST client here once it has a token.
- `guard` (`src/mcp/http.rs:26`): 403 unless Host is `127.0.0.1:<port>` or `localhost:<port>` (DNS rebinding); 401 unless `Authorization: Bearer <token>`, compared in constant time; fails closed on an empty token.
- `current_instructions` (`src/mcp/instructions.rs:72`): re-reads `~/.tritone/style.md` on every initialize and appends it as `## Writing style`.

## Dependencies
rmcp 3.8 (`server`, `macros`, `transport-streamable-http-server`; Cargo.lock pins 3.8.0), axum 0.8, tokio.

## Gotchas & invariants
- rmcp 3.8 rejects a `#[tool_router]` impl with no `#[tool]` fn unless it has `allow_empty` (rmcp-macros 3.8.0 `src/tool_router.rs:65`). The empty group routers carry it; drop it when the group gets its first tool.
- Don't import a type named `Result` with one generic parameter into a file with `#[tool_handler]` or `#[tool_router]`: the macros expand code that uses `Result<T, E>` unqualified. Import the domain alias as `DResult`.
- `#[tool_handler(router = self.tool_router)]` evaluates the expression on every call, so the merged router is built once and stored.
- Stateless mode answers `tools/call` without a prior `initialize`, which the HTTP test relies on.
- `AppState.api` is a `std::sync::RwLock` (so `ctx()` is sync); never hold it across an await.
- Halt contract (`src/mcp/state.rs:7`): a dead token calls `live.reject_token(token)`, any other halting error `live.halt(e)`; `ctx()` refuses while halted, across gateway reconnects.

## Testing
`cargo test --lib -- tools::read` runs the read and triage tools over HTTP against `FakeDiscord`; `cargo test tools::write` calls the write tool methods directly on a `TritoneServer` with a `FakeDiscord`. `cargo test mcp`: the guard via `tower::ServiceExt::oneshot`; `status` over HTTP through the guard (connecting, ready, needs_login, the login wake-up, and no wake-up for a rejected token); `run` refusing until the client is set and while halted, including across a RESUMED; the instructions and style guide.
