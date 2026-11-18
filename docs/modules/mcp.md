# mcp

Last verified: 2026-11-19

## Purpose
The MCP server Claude talks to: Streamable HTTP on `127.0.0.1:<port>/mcp` (default 7781). Stateless, behind a Host check and a bearer token. Tools are split into one router per group.

## Structure
- `src/mcp/server.rs`: `TritoneServer`, `tool_router()` (merges the group routers with `+`), `run`, `json`, and the `ServerHandler` impl.
- `src/mcp/tools/`: one `#[tool_router(router = <group>_router, vis = "pub(crate)")] impl TritoneServer` per file: `status.rs`, `read.rs`, `triage.rs`, `write.rs`, `engage.rs`, `activity.rs`.
- `src/mcp/state.rs`: `AppState`, `Ctx`.
- `src/mcp/http.rs`: `guard_router` (Host + bearer), `app` (the guarded `/mcp` service and `POST /login-notify`) and `serve` (graceful shutdown on SIGTERM/SIGINT).
- `src/mcp/instructions.rs`: `BASE_INSTRUCTIONS`, `build_instructions`, `read_style_guide`, `current_instructions`.
- Tests: `src/mcp/mcp_tests.rs`, `src/mcp/http_tests.rs`.

## Tools
- `status` (`src/mcp/tools/status.rs:26`): `connection` (`connecting`, `ready`, `reconnecting`, `needs_login`, `halted`), `user` (username only), `guilds` (a count), `note`. While halted on `NeedsLogin`, a keyring token that `AppState::accept_login` takes (`LiveState::try_login_with` accepts it: not the rejected one, nor one already tried) drops the old REST client and wakes the gateway through `login_notify`, and `status` reports `connecting`; the dead token never does, so polling `status` can't loop identifies (`src/mcp/tools/status.rs:45`).
- Write tools (`src/mcp/tools/write.rs`). None writes inside the call: each validates, then enqueues on the `SendQueue` with the current REST client and returns `{ job_id, position, eta_secs, note }`.
  - `send_message{channel, text, reply_to?, kind?}` (`src/mcp/tools/write.rs:107`), `react{channel, message, emoji}` (`:121`), `create_thread{channel, from_message?, name, text?}` (`:133`, text or announcement channels only), `create_forum_post{forum, title, text}` (`:151`, forum or media channels only).
  - `channel`/`forum` is an id or a `discord.com/channels/...` link of a channel `LiveState` knows (`target`, `src/mcp/tools/write.rs:65`). Outgoing text goes through `outgoing_text` (`src/mcp/tools/write.rs:82`), the one place `@username` → `<@id>` conversion plugs in.
  - `send_status{job_id?}` (`src/mcp/tools/write.rs:169`) and `resume_queue` (`src/mcp/tools/write.rs:180`) don't go through `run`, so they work while halted. `resume_queue` first attaches the REST client `AppState` holds now, then calls `SendQueue::resume`: refuses on `NeedsLogin`, otherwise lifts `live`'s and the client's halt and resumes the queue (after a block, writes still wait its Retry-After).
- Activity tools (`src/mcp/tools/activity.rs`):
  - `my_activity{days?}` (`src/mcp/tools/activity.rs:50`): the last `days` (default 30, at least 1) through `domain::activity::my_activity`. Reads only the store and `LiveState`, so it works while halted.
  - `backfill_activity` (`src/mcp/tools/activity.rs:62`): refuses until the connection is `Ready` (before READY no guilds are known). Unless every guild is done, starts `domain::activity::backfill` in a background task, at most one at a time (`BACKFILL_RUNNING`, `src/mcp/tools/activity.rs:22`); returns `{ running, guilds_done, guilds_total, recorded, messages_seen, last_error?, note }`. A run that stops stores its error in `kv` `backfill_last_error`; the next call continues it.
- Engage tools (`src/mcp/tools/engage.rs`):
  - Voice: `get_voice{channel}` (`src/mcp/tools/engage.rs:113`), through `run`, calls `domain::voice::get_voice` with `SearchPacer::global()`. `save_profile{category, body}` (`:125`), `save_chat_notes{channel, body}` (`:133`), `set_category{target, category}` (`:143`) and `list_stale_profiles` (`:150`) touch only the store and `LiveState`, so they work while halted (`local`, `src/mcp/tools/engage.rs:82`).
  - `channel` is an id, `<#id>` or a `discord.com/channels/...` link of a channel `LiveState` knows (`channel_arg`, `src/mcp/tools/engage.rs:94`); `target` may also be a server id (`target_arg`, `:100`).
  - Cleanup: `cleanup_plan` (`src/mcp/tools/engage.rs:159`) and `cleanup_apply{items[]}` (`:166`) run `domain::cleanup` through `run`. The descriptions and the instructions say each item needs the user's own explicit OK.
- The read and triage groups are empty so far.

## Other routes
- `POST /login-notify` (`src/mcp/http.rs:68`), behind the same guard: `tritone login` calls it after saving a token. Same rule as `status`: only while halted on `NeedsLogin` (any other halt, or a live connection → 409 asking for a restart, so a login can't lift a CAPTCHA or block halt or wake a healthy gateway); then `AppState::accept_login` with the keyring token, 200 when it acts; the rejected or already-tried token, or none → 409 with a "run `tritone login`" text; keyring error → 500. So a new login takes effect even if nobody polls `status`.

## Key methods
- `TritoneServer::tool_router` (`src/mcp/server.rs:38`): `status_router + read_router + triage_router + write_router + engage_router + activity_router`. Built once in `new` and kept in the struct.
- `TritoneServer::run` (`src/mcp/server.rs:49`): gets a `Ctx` from `AppState::ctx` and runs the action. Results are pretty JSON (`json`, `src/mcp/server.rs:25`). Errors come back as **tool errors** (`is_error: true`) with text Claude can act on; only the error kind is logged.
- `AppState::ctx` (`src/mcp/state.rs:94`): a halted connection returns its `DiscordError` (e.g. `NeedsLogin`, "run `tritone login`"); no REST client yet returns `NotConnected`.
- `AppState::accept_login(token)` (`src/mcp/state.rs:82`): the one place a login is accepted, shared by `status` and `/login-notify`: `try_login_with`, then `set_api(None)` (the old client holds the dead token), then `login_notify.notify_one()`. Callers check first that the connection is halted on `NeedsLogin`.
- `AppState::set_api` (`src/mcp/state.rs:74`): the gateway stores the REST client here once it has a token.
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
`cargo test mcp` (write tools: `cargo test tools::write`, calling the tool methods directly on a `TritoneServer` with a `FakeDiscord`): `/login-notify` (wakes on a new token only while halted on `NeedsLogin`; not on the rejected one, none, another halt, or a live connection; drops the old client; guarded); the guard via `tower::ServiceExt::oneshot`; `status` over HTTP through the guard (connecting, ready, needs_login, the login wake-up, and no wake-up for a rejected token); `run` refusing until the client is set and while halted, including across a RESUMED; the instructions and style guide.
