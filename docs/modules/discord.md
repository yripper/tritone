# discord

Last verified: 2026-11-18

## Purpose
Everything that talks to Discord. Nothing outside `src/discord/` makes network calls; the rest of tritone uses the `DiscordApi` trait and `LiveState`.

## Structure
- `src/discord/types.rs`: the slim model (`Id`, `ChannelKind`, `UserRef`, `Role`, `Overwrite`, `Guild`, `Channel`, `ReadState`, `MuteUntil`, `GuildSettings`, `Message`, `ReadySnapshot`), `DiscordError`, and the REST argument types (`OutgoingMessage`, `SearchScope`, `SearchQuery`, `SearchPage`, `MuteTarget`).
- `src/discord/api.rs`: `DResult` and the `DiscordApi` trait.
- `src/discord/fake.rs` (tests only): `FakeDiscord` and `Call`.
- `src/discord/gateway/mod.rs`: `spawn` (a stub that returns at once) and the `GatewayEvent` re-export; `src/discord/gateway/events.rs`: `GatewayEvent`.
- `src/discord/fingerprint.rs`: `Fingerprint` (super-properties, REST and WebSocket headers), the build-number scrape, the installation id.
- `src/discord/transport.rs`: `http_client` (wreq + Chrome emulation + cookie jar), `ws_connect`, `CookieJar`, `ChromeIdentity`.
- Tests: `src/discord/types_tests.rs`, `src/discord/fake_tests.rs`, `src/discord/fingerprint_tests.rs`, `src/discord/transport_tests.rs`.

## Public surface
- `Id(pub u64)` (`src/discord/types.rs:16`): `Copy + Eq + Hash + Ord + Default`. Serde writes a decimal string and reads a string or a number; `Display`/`FromStr`; `JsonSchema` (a digit string), so tool arguments can take it.
  - `timestamp_ms` (`src/discord/types.rs:20`): `(id >> 22) + 1420070400000`.
  - `from_timestamp_ms(ms, counter)` (`src/discord/types.rs:26`): builds an id for a time, e.g. a nonce; `counter` fills the low bits.
- `ChannelKind::from_u8` (`src/discord/types.rs:95`): 0 Text, 1 Dm, 2 Voice, 3 GroupDm, 4 Category, 5 Announcement, 10/11/12 threads, 15 Forum, 16 Media, else `Other(n)`. `is_thread()`, `is_private()` (DM or group DM).
- `UserRef::label` (`src/discord/types.rs:133`): display name, else username.
- `Guild` (`src/discord/types.rs:156`): `roles` includes @everyone, whose id is the guild id; `my_roles` are the user's roles.
- `MuteUntil::active(now_ms)` (`src/discord/types.rs:197`).
- `GuildSettings` (`src/discord/types.rs:209`): `guild_id: None` holds the DM settings; `channel_mutes` holds every channel override, including `No` (explicit unmute).
- `Message` (`src/discord/types.rs:218`): attachments are placeholders (`"image: a.png"`), embeds are titles, reactions are `(":name:" or emoji, count)`.
- `DiscordError` (`src/discord/types.rs:259`): `NeedsLogin`, `Captcha`, `Locked`, `Blocked`, `RateLimited`, `Indexing`, `Forbidden`, `NotFound`, `Api { status, code }`, `Http`. Display says what the user must do. `halts()` (`src/discord/types.rs:292`) is true for `NeedsLogin | Captcha | Locked | Blocked`: nothing retries those. `kind()` (`src/discord/types.rs:300`) is log-safe.
- `DiscordApi` (`src/discord/api.rs:13`): `history` (newest first, at most one of before/after), `around`, `recent_mentions`, `search`, `ack`, `ack_bulk` (≤100 items; callers chunk), `typing`, `send`, `react`, `create_thread`, `create_forum_post`, `leave_guild`, `set_mute`, `user`.
- `GatewayEvent` (`src/discord/gateway/events.rs:6`): `Ready`, `Resumed`, `MessageCreate`, `MessageAck`, `ChannelUpsert`, `ChannelDelete`, `GuildUpsert(guild, channels)`, `GuildDelete`, `SettingsUpdate`, `Disconnected`, `Halted`.

## Fingerprint (`src/discord/fingerprint.rs:44`)
Identity: Discord web in Chrome. Fields: `user_agent`, `browser_version`, `os`, `os_version`, `locale` (a Discord locale), `timezone` (IANA, `UTC` fallback), `build_number`, `installation_id`, plus `sec_ch_ua` and `sec_ch_ua_platform` (same Chrome profile as the UA).
- `Fingerprint::load(&Store, &dyn SecretStore)` (`src/discord/fingerprint.rs:144`): sync; reads the cached build (`kv` `build_number`, JSON `{"build","at"}`; falls back to `FALLBACK_BUILD` when missing or unreadable) and the installation id (`kv` `installation_id`). The keyring argument is unused today.
- `super_properties()` (`src/discord/fingerprint.rs:161`): `os`, `browser:"Chrome"`, `device:""`, `system_locale`, `browser_user_agent`, `browser_version`, `os_version`, the four empty referrer fields, `release_channel:"stable"`, `client_build_number`, `client_event_source:null`, in that order (serde_json `preserve_order`). Never `launch_signature` or `has_client_mods`. https://docs.discord.food/reference#client-properties
- `identify_properties()` (`src/discord/fingerprint.rs:182`): the same plus `installation_id` when known.
- `rest_headers(write)` (`src/discord/fingerprint.rs:196`): `Sec-Ch-Ua*`, `X-Discord-Locale`, `X-Discord-Timezone`, `X-Super-Properties` (standard base64 of compact JSON), `User-Agent`, `Accept: */*`, `X-Installation-ID` when known, `Origin: https://discord.com` on writes only, the `Sec-Fetch-*` trio (`same-origin`, `cors`, `empty`), `Referer: https://discord.com/channels/@me`, `Accept-Encoding`, `Accept-Language`, `Priority: u=1, i`. The REST client adds `Authorization`.
- `ws_headers()` (`src/discord/fingerprint.rs:227`): `Pragma`, `Cache-Control` (`no-cache`), `User-Agent`, `Origin: https://discord.com`, `Accept-Language`, capitalized.
- `refresh(client, store, now_ms)` (`src/discord/fingerprint.rs:240`): re-scrapes the build number when the cache is missing or older than 24 h (`BUILD_MAX_AGE_MS`), and fetches the installation id when unknown; failures keep the current values. `refresh_with` takes `Endpoints` so tests use a mock.
- `FALLBACK_BUILD` (`src/discord/fingerprint.rs:21`): 675174, read live on 2026-11-18.
- `refresh_build_number(client)` (`src/discord/fingerprint.rs:295`): `GET https://discord.com/app`, take the `/assets/sentry*.js` path (`find_sentry_asset`), fetch it, parse the digits after `buildNumber","` (`parse_build_number`). 5 s timeouts. `refresh_build_number_from(client, base)` for tests.
- `fetch_installation_id(client)` (`src/discord/fingerprint.rs:307`): unauthenticated `GET /api/v9/apex/experiments?surface=2`, field `installation`. https://docs.discord.food/topics/experiments
- `discord_locale(os_locale)` (`src/discord/fingerprint.rs:79`): nearest Discord locale (e.g. `es-CL` → `es-419`, `pt-PT` → `pt-BR`, unknown → `en-US`). `accept_language(locale)` (`src/discord/fingerprint.rs:112`): e.g. `es-419,es;q=0.9`.

## Transport (`src/discord/transport.rs`)
- `http_client(Arc<CookieJar>)` (`src/discord/transport.rs:80`): wreq with wreq-util's `Chrome149` profile for this platform (`CHROME_PROFILE`, `platform()`), **the profile's default headers off** (they are a page navigation's: `upgrade-insecure-requests`, `sec-fetch-dest: document`), the jar as cookie provider, 30 s timeout. Callers send `Fingerprint::rest_headers`.
- `chrome_identity()` (`src/discord/transport.rs:57`): the profile's UA, `sec-ch-ua` and platform, mirrored as constants; `chrome_identity_matches_the_wreq_util_profile` fails if wreq-util changes them.
- `ws_connect(client, url, headers)` (`src/discord/transport.rs:105`): `ws://`/`wss://` → an HTTP/1.1 GET through `client` (so ALPN is `http/1.1`, with Chrome TLS and the cookies), headers in Chrome's upgrade order with their casing preserved (`OrigHeaderMap`), our own `Sec-WebSocket-Key` and accept check; then `tokio_tungstenite::WebSocketStream::from_raw_socket` with `max_message_size`/`max_frame_size` = 256 MiB (`WS_MAX_SIZE`). Returns `WsStream = WebSocketStream<wreq::Upgraded>`. A non-101 answer is an error naming the status.
- `CookieJar` (`src/discord/transport.rs:144`): implements `wreq::cookie::CookieStore` over `cookie_store`; keeps only `discord.com` and `*.discord.com` cookies. `load(&dyn SecretStore)` reads keyring account `cookies` (base64 of cookie_store JSON; unreadable → empty). `save_if_dirty(secrets, now_ms)` writes at most every 5 s and only when changed; session cookies aren't persisted, like a browser restart. `spawn_saver(secrets, timer)` runs it every second.

## wreq 0.16.3 notes
- wreq's own `ws` feature is built on tokio-tungstenite 0.29, so tritone doesn't enable it: it does the upgrade with `Response::upgrade()` (always available) and frames with tokio-tungstenite 0.30 (`handshake` feature only, no TLS).
- Features used: `cookies`, `gzip`, `brotli`, `deflate`, `zstd` (responses are decompressed because tritone advertises them, as Chrome does).
- A request pinned to HTTP/1.1 makes wreq offer ALPN `http/1.1` only (wreq `src/tls/conn.rs`).

## FakeDiscord (`src/discord/fake.rs:37`)
- Records every call as a `Call` (`src/discord/fake.rs:17`); `calls()` returns them.
- Programmable: `history_pages` (`put_history`), `mentions`, `search_index` (filtered by scope, author, channel, min/max id, content; pages of `SEARCH_PAGE` = 25), `users`.
- `next_error` (`fail_next`): returned once by the next call, which is still recorded.
- `send` returns a `Message` with `id = nonce`, authored by `me`, records it in `sent` and adds it to the history. `create_thread` from a message takes the message's id; `create_forum_post` uses the nonce for the thread and its first message.
- `FakeDiscord::default()` posts as `Id(1)` / `U001`.

## Dependencies
serde, serde_json (`preserve_order`), schemars, thiserror, async-trait, wreq 0.16.3, wreq-util 0.2.1, tokio-tungstenite 0.30, cookie_store 0.22, url, base64, sys-locale, iana-time-zone. Building wreq compiles BoringSSL: cmake, a C/C++ compiler and libclang (see README).

## Gotchas & invariants
- Never log tokens, cookies or message text from here.
- The only live reads made while building this module were the two unauthenticated GETs above (`/app` + its sentry asset, and `apex/experiments`), once, to confirm their shapes. Tests never touch the network.
- `GuildUpsert` replaces the guild's whole channel set: the gateway must include the guild's joined threads in `channels`, or they're dropped.

## Testing
`cargo test discord`.
