# domain

Last verified: 2026-11-18

## Purpose
The logic between the MCP tools and Discord: errors, time, the live model and the send queue.

## Structure
- `src/domain/errors.rs`: `Error`, `Result`.
- `src/domain/clock.rs`: `Timer`, `GapRng`, `SystemTimer`, `ThreadRng`, `now_ms`, and `fake::{FakeTimer, SeqGaps}` for tests.
- `src/domain/live.rs`: `LiveState`, `BufferPolicy`, `ConnState` (a minimal model for now).
- `src/domain/send_queue.rs`: `SendQueue`, a stub with the final public signatures.

## Public surface
- `Error` (`src/domain/errors.rs:6`): `Discord(DiscordError)`, `Ambiguous { query, candidates }`, `NotFound`, `Invalid`, `NotConnected`, `Store(anyhow::Error)`. The Display text goes to Claude and says what to do next; `kind()` (`src/domain/errors.rs:25`) is log-safe.
- `Timer` (`src/domain/clock.rs:10`): `now_ms`, `sleep_until`, `now_secs`. `now_ms(&dyn Timer)` (`src/domain/clock.rs:21`).
- `fake::FakeTimer` (`src/domain/clock.rs:63`): sleeping moves virtual time forward. `fake::SeqGaps` (`src/domain/clock.rs:87`): hands out gaps in order, then repeats the last.
- `LiveState::new(BufferPolicy)` (`src/domain/live.rs:36`), `connection()` (starts `Connecting`), `me()`, `guilds()`.
- `SendQueue::new(store, timer, rng)`, `attach`, `run`, `enqueue` (`Err(Invalid("not implemented"))`), `status` (`src/domain/send_queue.rs:22` onward).

## Dependencies
chrono, rand, tokio, the `discord` types and the `store`.

## Gotchas & invariants
- `fake` is `#[cfg(test)]`: tests only.

## Testing
`cargo test domain`.
