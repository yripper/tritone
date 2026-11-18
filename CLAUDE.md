# tritone

Local daemon that keeps one live, browser-like connection to a personal Discord account and exposes an MCP server (Streamable HTTP on `127.0.0.1:7781`) so Claude can triage, read, find places to contribute, and answer as the user, paced and user-approved. Written in Rust. Runs under pm2.

## Hard rules

- **Public repo.** Never commit anything personal: no tokens, cookies, DB files, real Discord ids (17–20 digit snowflakes), names or message text, including in fixtures, tests and logs. Fixtures use small fake ids (`Id(1)`, `Id(101)`) and names like `G001`, `C001`, `U001`. Example topics are generic (`"rust"`, `"my-project"`). All runtime state lives in `~/.tritone/` (override with `TRITONE_HOME`; dir 0700, `tritone.db` 0600, WAL), never in the repo.
- Secrets live in the OS keyring, service `tritone`, accounts `token`, `bearer_token`, `cookies`: the macOS Keychain through `/usr/bin/security`, or the Linux Secret Service through `secret-tool`. Never in env files, config or on disk.
- Never log message content, tokens, cookies or the decrypted remote-auth user payload. Logs carry counts, kinds, status codes and opcodes only.
- No message text is stored on disk. SQLite holds triage state, voice notes, opportunity ids and the contribution log (ids only). Recent message text lives only in a bounded in-memory buffer.
- Only `src/discord/` makes network calls to Discord. Everything else goes through the `DiscordApi` trait or `LiveState`, so tests use fakes. Tests never touch the network: wiremock, local websocket servers, `FakeDiscord`.
- Identity: Discord web in Chrome (wreq-util Chrome profile). API `https://discord.com/api/v9`, gateway `wss://gateway.discord.gg/?v=9&encoding=json&compress=zlib-stream`, capabilities 253, no intents, presence `unknown` + `afk: true`. Never send `launch_signature` or `has_client_mods`. Never close the gateway with 1000/1001 except on logout or shutdown.
- Never solve or retry a CAPTCHA. Never make a REST call just to validate the token: READY means valid, close 4004 means invalid.
- Reading marks read unless `peek: true`. Every write (send, react, thread, forum post, leave, mute, bulk ack) needs the user's explicit OK, and sends go through the send queue.
- Code reuse: concord (GPL-3.0) may be read but never copied. Serein code may be adapted with a credit line in `THIRD_PARTY_NOTICES.md`. Protocol facts in docs cite public sources (docs.discord.food URLs).
- Before every commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass.

## Module Documentation Convention (MANDATORY)

Every module has a living doc at `docs/modules/<module>.md` (flat file, one per module). `docs/modules/README.md` is the index that routes a module's source path → its doc. These are the fast on-ramp for anyone — human or agent — touching a module.

**Progressive disclosure — do NOT load all docs at once.** When you're about to touch a module, open `docs/modules/README.md`, find the ONE doc matching the code you're changing, and read only that. Never pull the whole `docs/modules/` folder into context.

**The workflow rule:**
1. **Before modifying a module, read its `docs/modules/<module>.md` first.** It holds the file map, key methods with `file:line`, dependencies, and gotchas.
2. **After modifying a module, update its doc in the same change.** New/removed endpoints, changed behavior, new gotchas, dependency changes — all go into the doc before the work is done. Bump the "Last verified" date.
3. Doc claims must be verified against source and cite `file:line`. Never document something you haven't confirmed exists.
4. **Adding a new module?** Create its `docs/modules/<module>.md` and add a row to `docs/modules/README.md` in the same change.

Docs follow a shared template: Purpose, Structure, Endpoints/Public surface, Key methods (`file:line`), Dependencies, Gotchas & invariants, Testing.
