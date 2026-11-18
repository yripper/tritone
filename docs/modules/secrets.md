# secrets

Last verified: 2026-11-19

## Purpose
Secrets in the OS keyring, through its CLI so no native crate is needed. Service `tritone`.

## Structure
- `src/secrets.rs`: accounts, `CommandRunner`, `Cli`, `SecretStore`, `Keychain` (macOS), `SecretTool` (Linux), `default_store`, and `fake::MemSecrets` for tests.
- `src/secrets_tests.rs`: tests with a fake command runner.

## Public surface
- Accounts (`src/secrets.rs:9`): `ACCOUNT_TOKEN` (`token`, the Discord token), `ACCOUNT_BEARER` (`bearer_token`, the MCP bearer), `ACCOUNT_COOKIES` (`cookies`, the cookie jar).
- `SecretStore` (`src/secrets.rs:72`): async `get`, `set`, `delete`.
- `Keychain` (`src/secrets.rs:82`): `/usr/bin/security`. Exit 44 means not found.
- `SecretTool` (`src/secrets.rs:128`): `secret-tool`; values go on stdin, never argv.
- `default_store()` (`src/secrets.rs:176`): the store for this OS; other OSes are an error.
- `fake::MemSecrets` (`src/secrets.rs:209`, tests only).

## Dependencies
tokio (process), async-trait, anyhow.

## Gotchas & invariants
- `Keychain::set` writes through `security -i` on stdin, so values must be token characters (`assert_safe`, `src/secrets.rs:192`: letters, digits, `+ / = _ . : % -`). Anything else, like JSON, must be encoded first (e.g. base64). The whole `security -i` line must also fit in 4,000 bytes (`MAX_SECURITY_LINE`, `src/secrets.rs:80`): longer input would spill into a second command and store a truncated value, so `set` refuses it.
- No plaintext fallback: an OS without a keyring CLI fails loudly.

## Testing
`cargo test secrets`.
