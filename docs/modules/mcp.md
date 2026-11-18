# mcp

Last verified: 2026-11-19

## Purpose
The MCP server Claude talks to: Streamable HTTP on `127.0.0.1:<port>/mcp` (default 7781). Stateless, behind a Host check and a bearer token. Tools are split into one router per group.

## Structure
- `src/mcp/server.rs`: `TritoneServer`, `tool_router()` (merges the group routers with `+`), `run`, `json`, and the `ServerHandler` impl.
- `src/mcp/tools/`: one `#[tool_router(router = <group>_router, vis = "pub(crate)")] impl TritoneServer` per file: `status.rs`, `read.rs`, `triage.rs`, `write.rs`, `engage.rs`, `activity.rs`. Tests for the read and triage tools: `src/mcp/tools/read_tests.rs`.
- `src/mcp/state.rs`: `AppState`, `Ctx`.
- `src/mcp/http.rs`: `guard_router` (Host + bearer), `app` (the guarded `/mcp` service and `POST /login-notify`) and `serve` (graceful shutdown on SIGTERM/SIGINT).
- `src/mcp/instructions.rs`: `BASE_INSTRUCTIONS`, `build_instructions`, `read_style_guide`, `current_instructions`.
- Tests: `src/mcp/mcp_tests.rs`, `src/mcp/http_tests.rs`.

## Tools
- `status` (`src/mcp/tools/status.rs:26`): `connection` (`connecting`, `ready`, `reconnecting`, `needs_login`, `halted`), `user` (username only), `guilds` (a count), `note`. While halted on `NeedsLogin`, a keyring token that `AppState::accept_login` takes (`LiveState::try_login_with` accepts it: not the rejected one, nor one already tried) drops the old REST client and wakes the gateway through `login_notify`, and `status` reports `connecting`; the dead token never does, so polling `status` can't loop identifies (`src/mcp/tools/status.rs:45`).
- **Read** (`src/mcp/tools/read.rs`). Channel arguments go through `resolve_channel` (id, link, `#name`, `server/#name`, `@user`, part of a name); an ambiguous name is a tool error listing the candidates with ids, and nothing is read.
  - `list_servers{include_muted?}` (`src/mcp/tools/read.rs:101`) → `{servers}`.
  - `list_channels{kind? (dm|group_dm|server|thread), server?, watch?}` (`src/mcp/tools/read.rs:114`) → `{channels, truncated}`, at most 300.
  - `get_pending` (`src/mcp/tools/read.rs:130`) → `{pending: [{channel, label, reason, unread_count_hint, mention_count, messages, more_unread, pings?, error?}], truncated}` (`truncated`: the mentions inbox had more than 5 pages within 7 days). Never acks.
  - `read_history{channel, limit=50, before?, peek=false}` (`src/mcp/tools/read.rs:137`): marks read unless `peek` or `before`.
  - `read_thread{thread, limit=50, peek=false}` (`src/mcp/tools/read.rs:148`).
  - `search_messages{query, server?, channel?, author?, offset?}` (`src/mcp/tools/read.rs:160`): a server channel is searched through its server with `channel_id`; a DM through the channel endpoint; neither is an error. `author` goes through `resolve_user`. Offset capped at 9,975 (https://docs.discord.food/resources/message#search-guild-messages).
  - `get_channel_info{channel}` (`src/mcp/tools/read.rs:194`), `find_user{query}` (`src/mcp/tools/read.rs:206`) → `{users}`.
- **Triage** (`src/mcp/tools/triage.rs`):
  - `mark_read{channels[]}` (`src/mcp/tools/triage.rs:85`): every name must resolve first, or nothing is acked; → `{marked, already_read, not_found}`.
  - `snooze{channel, until}` (`src/mcp/tools/triage.rs:95`): `until` is RFC 3339 with offset; past times are refused. `unsnooze{channel}` (`src/mcp/tools/triage.rs:106`).
  - `set_watch_level{target, level: full|normal|muted|clear}` (`src/mcp/tools/triage.rs:119`): channel-shaped targets (`#`, `@`, `/#`, links) are channels; a bare id is whichever exists (one that is both a server and its default channel must be named instead); otherwise a server name, then a channel name (`resolve_target`, `src/mcp/tools/triage.rs:56`). → `{target, kind: server|channel, level}`.
- Write tools (`src/mcp/tools/write.rs`). None writes inside the call: each validates, then enqueues on the `SendQueue` with the current REST client and returns `{ job_id, position, eta_secs, note }`.
  - `send_message{channel, text, reply_to?, kind?}` (`src/mcp/tools/write.rs:108`), `react{channel, message, emoji}` (`:124`), `create_thread{channel, from_message?, name, text?}` (`:136`, text or announcement channels only), `create_forum_post{forum, title, text}` (`:154`, forum or media channels only).
  - `channel`/`forum` goes through `resolve_channel` (id, link, `#name`, `server/#name`, `@user`, part of a name) and must be a channel `LiveState` holds (`target`, `src/mcp/tools/write.rs:68`): several matches are an `Ambiguous` error listing them, never a guess, and unlike the read tools an unknown id is refused. Outgoing text goes through `outgoing_text` (`src/mcp/tools/write.rs:78`), which calls `render::to_discord_mentions` with the target channel and, for `send_message`, `reply_to` (only people seen in that server, or the DM's people, are converted). `send_message`, `create_thread` (with text) and `create_forum_post` add `final_text`, the text that will go out, to the enqueue result (`Queued`, `src/mcp/tools/write.rs:85`); the instructions say to show it when it differs from what the user approved.
  - `send_status{job_id?}` (`src/mcp/tools/write.rs:173`) and `resume_queue` (`src/mcp/tools/write.rs:184`) don't go through `run`, so they work while halted. `resume_queue` first attaches the REST client `AppState` holds now, then calls `SendQueue::resume`: refuses on `NeedsLogin`, otherwise lifts `live`'s and the client's halt and resumes the queue (after a block, writes still wait its Retry-After).
- Activity tools (`src/mcp/tools/activity.rs`):
  - `my_activity{days?}` (`src/mcp/tools/activity.rs:50`): the last `days` (default 30, at least 1) through `domain::activity::my_activity`. Reads only the store and `LiveState`, so it works while halted.
  - `backfill_activity` (`src/mcp/tools/activity.rs:62`): refuses until the connection is `Ready` (before READY no guilds are known). Unless every guild is done, starts `domain::activity::backfill` in a background task, at most one at a time (`BACKFILL_RUNNING`, `src/mcp/tools/activity.rs:22`); returns `{ running, guilds_done, guilds_total, recorded, messages_seen, last_error?, note }`. A run that stops stores its error in `kv` `backfill_last_error`; the next call continues it.
- Engage tools (`src/mcp/tools/engage.rs`):
  - Opportunities: `get_opportunities{limit?}` (`src/mcp/tools/engage.rs:122`, default 15, 1–50) through `run`; `dismiss_opportunity{id}` (`:135`) touches only the store.
  - Voice: `get_voice{channel}` (`src/mcp/tools/engage.rs:144`), through `run`, calls `domain::voice::get_voice` with `SearchPacer::global()`. `save_profile{category, body}` (`:156`), `save_chat_notes{channel, body}` (`:164`), `set_category{target, category}` (`:174`) and `list_stale_profiles` (`:181`) touch only the store and `LiveState`, so they work while halted (`local`, `src/mcp/tools/engage.rs:92`).
  - `channel` goes through `resolve_channel` (ids, links, `#name`, `server/#name`, `@user`, part of a name; `<#id>` is unwrapped first) and must be a channel `LiveState` knows (`channel_arg`, `src/mcp/tools/engage.rs:104`); `target` falls back to `resolve_server` when no channel matches (`target_arg`, `:110`).
  - Cleanup: `cleanup_plan` (`src/mcp/tools/engage.rs:190`) and `cleanup_apply{items[]}` (`:197`) run `domain::cleanup` through `run`. The descriptions and the instructions say each item needs the user's own explicit OK.

## Other routes
- `POST /login-notify` (`src/mcp/http.rs:68`), behind the same guard: `tritone login` calls it after saving a token. Same rule as `status`: only while halted on `NeedsLogin` (any other halt, or a live connection → 409 asking for a restart, so a login can't lift a CAPTCHA or block halt or wake a healthy gateway); then `AppState::accept_login` with the keyring token, 200 when it acts; the rejected or already-tried token, or none → 409 with a "run `tritone login`" text; keyring error → 500. So a new login takes effect even if nobody polls `status`.

## Key methods
- `TritoneServer::tool_router` (`src/mcp/server.rs:38`): `status_router + read_router + triage_router + write_router + engage_router + activity_router`. Built once in `new` and kept in the struct.
- `TritoneServer::run` (`src/mcp/server.rs:49`): gets a `Ctx` from `AppState::ctx` and runs the action. Results are pretty JSON (`json`, `src/mcp/server.rs:25`). Errors come back as **tool errors** (`is_error: true`) with text Claude can act on; only the error kind is logged.
- `AppState::ctx` (`src/mcp/state.rs:94`): a halted connection returns its `DiscordError` (e.g. `NeedsLogin`, "run `tritone login`"); no REST client yet returns `NotConnected`.
- `AppState::accept_login(token)` (`src/mcp/state.rs:82`): the one place a login is accepted, shared by `status` and `/login-notify`: `try_login_with`, then `set_api(None)` (the old client holds the dead token), then `login_notify.notify_one()`. Callers check first that the connection is halted on `NeedsLogin`.
- `AppState::set_api` (`src/mcp/state.rs:74`): the gateway stores the REST client here once it has a token.
- `guard` (`src/mcp/http.rs:26`): 403 unless Host is `127.0.0.1:<port>` or `localhost:<port>` (DNS rebinding); 401 unless `Authorization: Bearer <token>`, compared in constant time; fails closed on an empty token.
- `current_instructions` (`src/mcp/instructions.rs:81`): re-reads `~/.tritone/style.md` on every initialize and appends it as `## Writing style`.

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
`cargo test --lib -- tools::read` runs the read and triage tools over HTTP against `FakeDiscord`. `cargo test mcp` (write tools: `cargo test tools::write`, calling the tool methods directly on a `TritoneServer` with a `FakeDiscord`): `/login-notify` (wakes on a new token only while halted on `NeedsLogin`; not on the rejected one, none, another halt, or a live connection; drops the old client; guarded); the guard via `tower::ServiceExt::oneshot`; `status` over HTTP through the guard (connecting, ready, needs_login, the login wake-up, and no wake-up for a rejected token); `run` refusing until the client is set and while halted, including across a RESUMED; the instructions and style guide.
