# mcp

Last verified: 2026-11-18

## Purpose
The MCP server Claude talks to: Streamable HTTP on `127.0.0.1:<port>/mcp` (default 7781). Stateless, behind a Host check and a bearer token. Tools are split into one router per group.

## Structure
- `src/mcp/server.rs`: `TritoneServer`, `tool_router()` (merges the group routers with `+`), `run`, `json`, and the `ServerHandler` impl.
- `src/mcp/tools/`: one `#[tool_router(router = <group>_router, vis = "pub(crate)")] impl TritoneServer` per file: `status.rs`, `read.rs`, `triage.rs`, `write.rs`, `engage.rs`, `activity.rs`.
- `src/mcp/state.rs`: `AppState`, `Ctx`.
- `src/mcp/http.rs`: `guard_router` (Host + bearer), `app` (the guarded `/mcp` service) and `serve` (graceful shutdown on SIGTERM/SIGINT).
- `src/mcp/instructions.rs`: `BASE_INSTRUCTIONS`, `build_instructions`, `read_style_guide`, `current_instructions`.
- Tests: `src/mcp/mcp_tests.rs`, `src/mcp/http_tests.rs`.

## Tools
- `status` (`src/mcp/tools/status.rs:26`): `connection` (`connecting`, `ready`, `reconnecting`, `needs_login`, `halted`), `user` (username only), `guilds` (a count), `note`. While halted on `NeedsLogin`, a keyring token that `LiveState::try_login_with` accepts (not the rejected one, nor one already tried) wakes the gateway through `login_notify` and reports `connecting`; the dead token never does, so polling `status` can't loop identifies (`src/mcp/tools/status.rs:45`).
- Write tools (`src/mcp/tools/write.rs`). None writes inside the call: each validates, then enqueues on the `SendQueue` with the current REST client and returns `{ job_id, position, eta_secs, note }`.
  - `send_message{channel, text, reply_to?, kind?}` (`src/mcp/tools/write.rs:107`), `react{channel, message, emoji}` (`:121`), `create_thread{channel, from_message?, name, text?}` (`:133`, text or announcement channels only), `create_forum_post{forum, title, text}` (`:151`, forum or media channels only).
  - `channel`/`forum` is an id or a `discord.com/channels/...` link of a channel `LiveState` knows (`target`, `src/mcp/tools/write.rs:65`). Outgoing text goes through `outgoing_text` (`src/mcp/tools/write.rs:82`), the one place `@username` → `<@id>` conversion plugs in.
  - `send_status{job_id?}` (`src/mcp/tools/write.rs:169`) and `resume_queue` (`src/mcp/tools/write.rs:180`) don't go through `run`, so they work while halted. `resume_queue` calls `SendQueue::resume`: refuses on `NeedsLogin`, otherwise lifts the halt and resumes the queue.
- The read, triage, engage and activity groups are empty so far.

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
`cargo test mcp` (write tools: `cargo test tools::write`, calling the tool methods directly on a `TritoneServer` with a `FakeDiscord`): the guard via `tower::ServiceExt::oneshot`; `status` over HTTP through the guard (connecting, ready, needs_login, the login wake-up, and no wake-up for a rejected token); `run` refusing until the client is set and while halted, including across a RESUMED; the instructions and style guide.
