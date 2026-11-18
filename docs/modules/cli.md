# cli

Last verified: 2026-11-18

## Purpose
The `tritone` binary: `start` (the daemon pm2 runs), `login [--token]`, `logout`, `status`, `token`.

## Structure
- `src/main.rs`: tracing setup (`tritone=info`, no ANSI under pm2) on a current-thread tokio runtime.
- `src/cli/mod.rs`: clap `Cli`/`Command`, `status`, `run`.
- `src/cli/daemon.rs`: `ensure_bearer_token`, `buffer_policy`, `start`.
- `src/cli/cli_tests.rs`: tests.

## Public surface
- `Command` (`src/cli/mod.rs:23`). `login` and `logout` print "not implemented yet" for now.
- `status` (`src/cli/mod.rs:49`): port, whether the bearer and the Discord token are stored. It never contacts Discord.
- `ensure_bearer_token` (`src/cli/daemon.rs:21`): 32 random bytes as hex, created once in the keyring.
- `start` (`src/cli/daemon.rs:39`), in order:
  1. binds `127.0.0.1:<port>`, which is the single-instance guard (a second daemon must never open a second gateway session);
  2. ensures the bearer token;
  3. opens the store and builds `AppState` (`LiveState` with `buffer_policy`, `SendQueue`, `SystemTimer`);
  4. spawns `discord::gateway::spawn(state)` and `queue.run()`;
  5. serves MCP.

## Dependencies
clap, rand, tokio, the `config`, `secrets`, `store`, `domain`, `mcp` and `discord` modules.

## Gotchas & invariants
- The bearer is hex, not base64: a value starting with `-` could be read as a flag inside the `security -i` command line.

## Testing
`cargo test cli`.
