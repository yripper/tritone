# config

Last verified: 2026-11-19

## Purpose
Runtime paths and the optional user config. All state lives in `~/.tritone/` (or `TRITONE_HOME`), never in the repo.

## Structure
- `src/config.rs`: `DEFAULT_PORT`, `Paths`, `Topic`, `Config`.
- `src/config_tests.rs`: tests.

## Public surface
- `DEFAULT_PORT = 7781` (`src/config.rs:10`).
- `Paths::resolve()` (`src/config.rs:21`): `home`, `db` (`tritone.db`), `config` (`config.json`), `style` (`style.md`). Creates the home dir and sets it to 0700.
- `Topic { kind, keywords }` (`src/config.rs:39`): `kind` labels the opportunities and log entries the topic produces.
- `Config` (`src/config.rs:47`), every field optional (`#[serde(default)]`):
  - `port` (7781)
  - `topics` (empty: the user fills them in)
  - `unanswered_after_mins` (30), `unanswered_max_age_hours` (48)
  - `recent_per_channel` (200), `recent_max_age_hours` (24)
- `Config::load(&Paths)` (`src/config.rs:74`): a missing file means defaults; a malformed one is an error naming the file.

Example `~/.tritone/config.json`:
```json
{ "topics": [ { "kind": "rust", "keywords": ["rust", "cargo", "tokio"] } ] }
```

## Dependencies
serde, serde_json, anyhow.

## Gotchas & invariants
- Env vars are process-global, so only one test touches `TRITONE_HOME`; the others build `Paths` by hand.

## Testing
`cargo test config`.
