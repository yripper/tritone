# discord

Last verified: 2026-11-18

## Purpose
Everything that talks to Discord. Nothing outside `src/discord/` makes network calls; the rest of tritone uses the `DiscordApi` trait and `LiveState`.

## Structure
- `src/discord/types.rs`: `Id`, `UserRef`, `Role`, `Guild`, `DiscordError`.
- `src/discord/api.rs`: `DResult` and the `DiscordApi` trait (empty for now).
- `src/discord/gateway/mod.rs`: `spawn`, a stub that returns at once.

## Public surface
- `Id(pub u64)` (`src/discord/types.rs:5`).
- `DiscordError` (`src/discord/types.rs:36`): `NeedsLogin`, `Captcha`, `Locked`, `Blocked`, `RateLimited`, `Indexing`, `Forbidden`, `NotFound`, `Api`, `Http`. The Display text says what the user must do; `kind()` (`src/discord/types.rs:69`) is a log-safe label.

## Dependencies
thiserror, async-trait.

## Gotchas & invariants
- Never log tokens, cookies or message text from here.

## Testing
Covered through `cargo test` in the modules that use these types.
