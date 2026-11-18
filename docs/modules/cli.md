# cli

Last verified: 2026-11-19

## Purpose
The `tritone` binary: `start` (the daemon pm2 runs), `login [--token]`, `logout`, `status`, `token`.

## Structure
- `src/main.rs`: tracing setup (`tritone=info`, no ANSI under pm2) on a current-thread tokio runtime.
- `src/cli/mod.rs`: clap `Cli`/`Command`, `status`, `run`.
- `src/cli/login.rs`: `login`, `logout`, `render_qr`, `save_login`, `forget_login`, `notify_daemon`.
- `src/cli/daemon.rs`: `ensure_bearer_token`, `buffer_policy`, `start`.
- Tests: `src/cli/cli_tests.rs`, `src/cli/login_tests.rs`.

## Public surface
- `Command` (`src/cli/mod.rs:24`).
- `login(paste)` (`src/cli/login.rs:117`): `tritone login` runs the QR flow (`discord::login::qr_login`) with the daemon's cached fingerprint and the saved cookie jar, printing the QR as unicode blocks (`render_qr`, `src/cli/login.rs:35`: EC level M, quiet zone, light on dark). On `Captcha` it prints how to copy a token from the browser's devtools (Network → any `/api/` request → `Authorization`) and falls back to a hidden prompt (`rpassword`); `--token` goes straight to the prompt. Pasted tokens go through `validate_pasted`. Other failures end with a hint to retry or use `--token`.
- `save_login` (`src/cli/login.rs:54`): keyring `token`, plus `cookies` when the login changed the jar. Prints `logged in` (no user details).
- Then it tells the daemon: `notify_daemon(port, bearer)` (`src/cli/login.rs:73`) sends `POST /login-notify` over plain HTTP/1.1 to `127.0.0.1:<port>` with the MCP bearer; the daemon's answer is printed. Not running (connection refused) → "start tritone"; no bearer yet → tritone never ran.
- `logout` (`src/cli/login.rs:150`) / `forget_login` (`src/cli/login.rs:61`): deletes keyring `token` and `cookies`; the MCP bearer stays. A running daemon keeps its connection until restarted.
- `status` (`src/cli/mod.rs:49`): port, whether the bearer and the Discord token are stored. It never contacts Discord.
- `ensure_bearer_token` (`src/cli/daemon.rs:22`): 32 random bytes as hex, created once in the keyring.
- `start` (`src/cli/daemon.rs:40`), in order:
  1. binds `127.0.0.1:<port>`, which is the single-instance guard (a second daemon must never open a second gateway session);
  2. ensures the bearer token;
  3. opens the store and builds `AppState` (`LiveState` with `buffer_policy`, `SendQueue`, `SystemTimer`);
  4. spawns the contribution log recorder (`domain::activity::spawn_recorder`) and the opportunity scanner (`domain::opportunities::spawn_scanner`, `src/cli/daemon.rs:59`), both before the gateway so they see every event, `discord::gateway::spawn(state)` and `queue.run()`;
  5. serves MCP.

## Dependencies
clap, rand, tokio, qrcode, rpassword, the `config`, `secrets`, `store`, `domain`, `mcp` and `discord` modules.

## Gotchas & invariants
- `tritone login` runs in its own process: the daemon learns about the new token through `/login-notify` and reloads the cookie jar (`CookieJar::reload`) before building the new REST client.
- The bearer is hex, not base64: a value starting with `-` could be read as a flag inside the `security -i` command line.

## Testing
`cargo test cli`.
