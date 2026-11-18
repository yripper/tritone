# discord

Last verified: 2026-11-19

## Purpose
Everything that talks to Discord. Nothing outside `src/discord/` makes network calls; the rest of tritone uses the `DiscordApi` trait and `LiveState`.

## Structure
- `src/discord/types.rs`: the slim model (`Id`, `ChannelKind`, `UserRef`, `Role`, `Overwrite`, `Guild`, `Channel`, `ReadState`, `MuteUntil`, `GuildSettings`, `Message`, `ReadySnapshot`), `DiscordError`, and the REST argument types (`OutgoingMessage`, `SearchScope`, `SearchQuery`, `SearchPage`, `MuteTarget`).
- `src/discord/api.rs`: `DResult` and the `DiscordApi` trait.
- `src/discord/fake.rs` (tests only): `FakeDiscord` and `Call`.
- `src/discord/gateway/`: the live connection and `GatewayEvent`; see [gateway.md](gateway.md).
- `src/discord/fingerprint.rs`: `Fingerprint` (super-properties, REST and WebSocket headers), the build-number scrape, the installation id.
- `src/discord/transport.rs`: `http_client` (wreq + Chrome emulation + cookie jar), `ws_connect`, `CookieJar`, `ChromeIdentity`.
- `src/discord/http.rs`: `HttpDiscord`, the real `DiscordApi` (REST over the wreq client), and `rebuild_api`.
- `src/discord/login.rs`: the remote-auth QR login and `validate_pasted`.
- `src/discord/rate.rs`: `Pacer` (one request in flight, gaps, 429 cooldown).
- `src/discord/parse.rs`: Discord JSON to the model (`parse_message`, `parse_user`, `parse_channel`), shared by REST and the gateway.
- Tests: `src/discord/http_tests.rs` (wiremock), `src/discord/login_tests.rs` (a local remote-auth WebSocket server plus wiremock), `src/discord/parse_tests.rs`, `src/discord/types_tests.rs`, `src/discord/fake_tests.rs`, `src/discord/fingerprint_tests.rs`, `src/discord/transport_tests.rs`.

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
- `DiscordError` (`src/discord/types.rs:259`): `NeedsLogin`, `Captcha`, `Locked`, `Blocked`, `RateLimited`, `Indexing`, `Forbidden`, `NotFound`, `Api { status, code }`, `Http`, `GatewayClosed { code }` (a fatal gateway close: restart tritone), `SessionRefused` (3 identifies in a row without READY: restart tritone). Display says what the user must do. `halts()` (`src/discord/types.rs:300`) is true for `NeedsLogin | Captcha | Locked | Blocked | GatewayClosed | SessionRefused`: nothing retries those. `kind()` (`src/discord/types.rs:313`) is log-safe.
- `DiscordApi` (`src/discord/api.rs:13`): `history` (newest first, at most one of before/after), `around`, `recent_mentions`, `search`, `ack`, `ack_bulk` (≤100 items; callers chunk), `typing`, `send`, `react`, `create_thread`, `create_forum_post`, `leave_guild`, `set_mute`, `user`, and `clear_halt()` (`src/discord/api.rs:35`), a sync default no-op: a client with its own sticky halt overrides it; only `SendQueue::resume` calls it, on the user's explicit OK.
- `GatewayEvent` (`src/discord/gateway/events.rs:6`): `Ready`, `Resumed`, `MessageCreate`, `MessageAck`, `ChannelUpsert`, `ChannelDelete`, `GuildUpsert(guild, channels)` (GUILD_CREATE), `GuildUpdate(guild)` (GUILD_UPDATE: keeps channels and my roles), `MyRolesUpdate { guild, roles }` (GUILD_MEMBER_UPDATE for self), `RoleUpsert { guild, role }` / `RoleDelete { guild, role }` (GUILD_ROLE_*), `GuildDelete`, `SettingsUpdate`, `Disconnected`, `Halted`. On close 4004 the gateway calls `LiveState::reject_token` rather than sending `Halted(NeedsLogin)`, so the dead token is remembered.

## HttpDiscord (`src/discord/http.rs`)
- `HttpDiscord::new(base, token, fp, client, live)` (`src/discord/http.rs:124`): `base` is the API root (`API_BASE` = `https://discord.com/api/v9`, `src/discord/http.rs:32`; wiremock in tests). `live` is the halt sink.
- Every request: `Fingerprint::rest_headers(write)` with `Authorization: <token>` (no prefix), `Content-Type: application/json` when there's a body, and any request header (`X-Context-Properties`) inserted after `Sec-Ch-Ua-Platform` (`headers`, `src/discord/http.rs:177`). Path segments are percent-encoded one by one (`url::Url::path_segments_mut`).
- `call` (`src/discord/http.rs:189`), in order: `stopped()` (`src/discord/http.rs:136`: this client's halt slot, else `LiveState`'s halt, so a gateway 4004 or CAPTCHA also stops REST; set → return that error, no request); the `Pacer` turn (held for the whole request, retry included); `stopped()` again, because calls queued behind a request that just halted must not go out; send; `classify` (`src/discord/http.rs:277`):
  - 2xx → ok (202 kept for `search`);
  - JSON `code` 40002/40004 → `Locked`; 40333 → `Blocked`;
  - 400 with `captcha_key` → `Captcha`; 401 with code 60003 (the action needs MFA) → `Locked`, keeping the token; any other 401 → `NeedsLogin`;
  - 403/429 whose body isn't JSON (Cloudflare HTML or text) → `Blocked`; JSON 403 → `Forbidden`; 404 → `NotFound`;
  - 429 with `retry_after` (body, else the `Retry-After` header) → the client cools down that long; a GET is retried once when the wait is ≤ 10 s (`MAX_GET_RETRY_WAIT`), otherwise `RateLimited`. Writes are never retried. 429 without any wait → `Blocked` (never retry blind).
  - `Blocked.retry_after_s` is the `Retry-After` header, at least 60. A `Blocked` answer also cools the pacer down for that long, so clearing the halt early (`clear_halt`) still sends nothing before the block has passed: calls get `RateLimited`.
  - https://docs.discord.food/topics/rate-limits, https://docs.discord.food/topics/captcha-handling
- Halts (`set_halt`, `src/discord/http.rs:145`): any error with `halts()` is stored (first wins) and reported: `NeedsLogin` → `live.reject_token(token)`, others → `live.halt(e)`. Logs carry the kind only.
- Endpoints (https://docs.discord.food/resources/message, /resources/channel, /topics/read-state, /resources/user-settings):
  - `history`/`around`: `GET /channels/{c}/messages?limit=1..100` with `before`, else `after`, or `around`.
  - `recent_mentions`: `GET /users/@me/mentions?limit&roles=true&everyone=true[&before]`.
  - `search` (`src/discord/http.rs:376`): `GET /guilds/{g}/messages/search` or `/channels/{c}/messages/search` with `content`, `author_id`, `channel_id`, `min_id`, `max_id`, `offset` (when > 0). Each result group yields its `hit:true` message (else the first). 202 → `Indexing` (`retry_after` 0 → 5 s).
  - `ack`: `POST /channels/{c}/messages/{m}/ack` `{"token":null,"last_viewed":<days since 2015-01-01>}`. `ack_bulk`: `POST /read-states/ack-bulk` `{read_states:[{read_state_type:0,channel_id,message_id}]}`, chunked by 100; empty → no request.
  - `typing`: `POST /channels/{c}/typing`.
  - `send` (`src/discord/http.rs:429`): `{content, nonce, tts:false, flags: 0|4096 (silent), mobile_network_type:"unknown", message_reference?: {message_id, channel_id, guild_id? (from LiveState), fail_if_not_exists:false}}` in that order, header `X-Context-Properties` = base64 `{"location":"chat_input"}`.
  - `react`: `PUT .../reactions/{emoji}/@me`; `<:name:id>`/`<a:name:id>`/`:name:id` → `name:id`, unicode is percent-encoded.
  - `create_thread`: `POST /channels/{c}/messages/{m}/threads {name, auto_archive_duration:1440}` or `POST /channels/{c}/threads {name, type:11, auto_archive_duration:1440}`.
  - `create_forum_post` (`src/discord/http.rs:483`): `POST /channels/{forum}/threads {name, applied_tags:[], message:{content}}`; the answer's `message` is the first post. The nonce is not sent: the forum message object has no nonce field.
  - `leave_guild`: `DELETE /users/@me/guilds/{g}` `{lurking:false}`.
  - `set_mute` (`src/discord/http.rs:498`): `PATCH /users/@me/guilds/{g|@me}/settings`; guild `{muted, mute_config}`, channel `{channel_overrides:{"<c>":{muted, mute_config}}}` (a map here, an array in READY). `mute_config` is `{end_time:null, selected_time_window:-1}` when muting, `null` when unmuting.
  - `user` (`src/discord/http.rs:512`): `GET /users/{u}/profile?with_mutual_guilds=false`, reads `.user`.
- `rebuild_api(jar, secrets, fp, live, token)` (`src/discord/http.rs:524`): `jar.reload(secrets)`, a fresh `http_client(jar)`, an `HttpDiscord` on `API_BASE`. Returns the API and the client (the gateway's WebSocket uses the same client). The daemon calls it on the first connect and whenever a new login wakes it.

## Login (`src/discord/login.rs`)
Remote-auth v2, https://docs.discord.food/remote-authentication/desktop.
- `qr_login(client, fp, show)` (`src/discord/login.rs:71`) → the token. `qr_login_with(.., &LoginEndpoints, KeySource)` (`src/discord/login.rs:80`) takes the endpoints (`REMOTE_AUTH_URL` = `wss://remote-auth-gateway.discord.gg/?v=2`, the API root) and the RSA key source, so tests use local servers and one cached key.
- Before the socket: unauthenticated `GET {api}/experiments` with `rest_headers(false)`; its `fingerprint` becomes `X-Fingerprint` on the ticket exchange (`fetch_x_fingerprint`, `src/discord/login.rs:115`; https://docs.discord.food/authentication#fingerprints). A failure is tolerated.
- One session (`session`, `src/discord/login.rs:145`): a fresh RSA-2048 key; `ws_connect` with `fp.ws_headers()` (carries `Origin: https://discord.com`, which the gateway requires); `hello` → heartbeat every `heartbeat_interval` and `init {encoded_public_key: base64(SPKI DER)}`; `nonce_proof {encrypted_nonce}` → OAEP-SHA256 decrypt → `{op:"nonce_proof", nonce: base64url-no-pad}` (`nonce_proof`, `src/discord/login.rs:134`); `pending_remote_init {fingerprint}` must equal base64url-no-pad SHA-256 of our SPKI, else the socket closes and the login fails without showing a QR; then `show("https://discord.com/ra/<fingerprint>")`; `pending_ticket` is only logged as "scanned" (its payload is never decrypted); `pending_login {ticket}` ends the session; `cancel` → `Cancelled`.
- Close 4003 (expired), a missed heartbeat ack, or `timeout_ms` + 10 s without a ticket → a new session with a new key, 1 s later, up to 3 times (`REGENERATIONS`), then `Expired`. Any other close code → `Protocol`.
- Ticket exchange (`exchange_ticket`, `src/discord/login.rs:243`): `POST {api}/users/@me/remote-auth/login {"ticket"}`, no `Authorization`, `rest_headers(true)` plus `Content-Type` and `X-Fingerprint`, same client (same cookies). A body with `captcha_key` → `Captcha` (never solved or retried); other non-2xx → `Http`; `encrypted_token` → OAEP-SHA256 decrypt → the token.
- `LoginError` (`src/discord/login.rs:33`): `Captcha`, `Cancelled`, `Expired`, `InvalidToken(reason)`, `Protocol`, `Http`.
- `validate_pasted(raw)` (`src/discord/login.rs:288`): trims whitespace and one pair of `"` or `'`; 16..=2048 bytes; no `Bot `/`Bearer ` prefix; graphic ASCII without quotes (rules as Serein's `crates/client-core/src/auth.rs`).
- Never logged: the ticket, the token, the user payload.

## Pacer (`src/discord/rate.rs`)
- `Pacer::turn(write)` (`src/discord/rate.rs:50`): one turn at a time (a tokio mutex held for the request). Waits until 250 ms (`MIN_GAP`) after the last request ended, and 500 ms (`WRITE_GAP`) after the last write. During a cooldown it returns `Err(seconds left)` at once and the caller answers `RateLimited` without a request.
- `Turn::cool_down(wait)` (`src/discord/rate.rs:69`): set on a 429. Discord bans an IP for 24 h after 10,000 401/403/429 answers in 10 minutes (https://docs.discord.food/topics/rate-limits), so one 429 stops every caller.

## Parsing (`src/discord/parse.rs`)
`pub(crate)`, shared by the REST client and the gateway; REST and gateway objects have the same shape. https://docs.discord.food/resources/message, https://docs.discord.food/resources/channel
- `parse_message(&Value) -> Option<Message>` (`src/discord/parse.rs:72`): `None` without id, channel or author. `timestamp` (RFC 3339) → ms, else the id's time; `edited` = `edited_timestamp` not null; `reply_to` from `message_reference.message_id`, `reply_to_author` from `referenced_message.author.id` (null when deleted); attachments → `"image: a.png"` (content type `image/*`, else a known image extension) or `"file: b.pdf"`; embeds → titles, untitled ones dropped; reactions → `(":name:", n)` for custom emoji, `("👍", n)` for unicode.
- `parse_user` (`src/discord/parse.rs:39`): `global_name` is the display name (empty → `None`).
- `parse_channel` (`src/discord/parse.rs:121`): `None` without id or type; overwrite `type` 0/1 or `"role"`/`"member"`, `allow`/`deny` as decimal strings. `recipients` must be full user objects (READY's `recipient_ids` are the gateway's job).

## Fingerprint (`src/discord/fingerprint.rs:46`)
Identity: Discord web in Chrome. Fields: `user_agent`, `browser_version`, `os`, `os_version`, `locale` (a Discord locale), `timezone` (IANA, `UTC` fallback), `build_number`, `installation_id`, plus `sec_ch_ua` and `sec_ch_ua_platform` (same Chrome profile as the UA).
- `Fingerprint::load(&Store, &dyn SecretStore)` (`src/discord/fingerprint.rs:146`): sync; `for_this_machine()` (`src/discord/fingerprint.rs:155`) plus the cached build (`kv` `build_number`, JSON `{"build","at"}`; falls back to `FALLBACK_BUILD` when missing or unreadable) and the installation id (`kv` `installation_id`). The keyring argument is unused today.
- `super_properties()` (`src/discord/fingerprint.rs:172`): `os`, `browser:"Chrome"`, `device:""`, `system_locale`, `browser_user_agent`, `browser_version`, `os_version`, the four empty referrer fields, `release_channel:"stable"`, `client_build_number`, `client_event_source:null`, in that order (serde_json `preserve_order`). Never `launch_signature` or `has_client_mods`. https://docs.discord.food/reference#client-properties
- `identify_properties()` (`src/discord/fingerprint.rs:193`): the same plus `installation_id` when known.
- `rest_headers(write)` (`src/discord/fingerprint.rs:207`): `Sec-Ch-Ua*`, `X-Discord-Locale`, `X-Discord-Timezone`, `X-Super-Properties` (standard base64 of compact JSON), `User-Agent`, `Accept: */*`, `X-Installation-ID` when known, `Origin: https://discord.com` on writes only, the `Sec-Fetch-*` trio (`same-origin`, `cors`, `empty`), `Referer: https://discord.com/channels/@me`, `Accept-Encoding`, `Accept-Language`, `Priority: u=1, i`. The REST client adds `Authorization`.
- `ws_headers()` (`src/discord/fingerprint.rs:274`): `Pragma`, `Cache-Control` (`no-cache`), `User-Agent`, `Origin: https://discord.com`, `Accept-Language`, capitalized.
- `refresh(client, store, now_ms)` (`src/discord/fingerprint.rs:287`): re-scrapes the build number when the cache is missing or older than 24 h (`BUILD_MAX_AGE_MS`), and fetches the installation id when unknown; failures keep the current values. `refresh_with` takes `Endpoints` so tests use a mock.
- `FALLBACK_BUILD` (`src/discord/fingerprint.rs:21`): 675174, read live on 2026-11-18.
- `refresh_build_number(client)` (`src/discord/fingerprint.rs:348`): `GET https://discord.com/app` with Chrome's page-navigation headers, take the `/assets/sentry*.js` path (`find_sentry_asset`), fetch it with Chrome's script headers (`Referer: …/app`), parse the digits after `buildNumber","` (`parse_build_number`). 5 s timeouts. `refresh_build_number_from(client, &Fingerprint, base)` (`src/discord/fingerprint.rs:352`) for tests and for `refresh`.
- `fetch_installation_id(client)` (`src/discord/fingerprint.rs:361`): unauthenticated `GET /api/v9/apex/experiments?surface=2` with `rest_headers(false)`, field `installation`. `fetch_installation_id_from(client, &Fingerprint, base)` (`src/discord/fingerprint.rs:365`). https://docs.discord.food/topics/experiments
- `discord_locale(os_locale)` (`src/discord/fingerprint.rs:81`): nearest Discord locale (e.g. `es-CL` → `es-419`, `pt-PT` → `pt-BR`, unknown → `en-US`). `accept_language(locale)` (`src/discord/fingerprint.rs:114`): e.g. `es-419,es;q=0.9`.

## Transport (`src/discord/transport.rs`)
- `http_client(Arc<CookieJar>)` (`src/discord/transport.rs:85`): wreq with wreq-util's `Chrome149` profile for this platform (`CHROME_PROFILE`, `platform()`), **the profile's default headers off** (they are a page navigation's: `upgrade-insecure-requests`, `sec-fetch-dest: document`), the jar as cookie provider, 30 s timeout. Every request must carry its own headers: `Fingerprint::rest_headers` for API calls, `ws_headers` for the gateway (the fingerprint's own scrape sends page/script headers).
- `chrome_identity()` (`src/discord/transport.rs:62`): the profile's UA, `sec-ch-ua` and platform, mirrored as constants; `chrome_identity_matches_the_wreq_util_profile` fails if wreq-util changes them.
- `ws_connect(client, url, headers)` (`src/discord/transport.rs:110`): `ws://`/`wss://` → an HTTP/1.1 GET through `client` (so ALPN is `http/1.1`, with Chrome TLS and the cookies), headers in Chrome's upgrade order with their casing preserved (`OrigHeaderMap`), our own `Sec-WebSocket-Key` and accept check; then `tokio_tungstenite::WebSocketStream::from_raw_socket` with `max_message_size`/`max_frame_size` = 256 MiB (`WS_MAX_SIZE`). Returns `WsStream = WebSocketStream<wreq::Upgraded>`. A non-101 answer is an error naming the status.
- `CookieJar` (`src/discord/transport.rs:150`): implements `wreq::cookie::CookieStore` over `cookie_store`; keeps only `discord.com` and `*.discord.com` cookies. `load(&dyn SecretStore)` (`src/discord/transport.rs:179`) reads keyring account `cookies` (base64 of deflated cookie_store JSON; unreadable → empty). `reload(&dyn SecretStore)` (`src/discord/transport.rs:197`) replaces the contents in place from the keyring and clears `dirty`, so the daemon's one jar (and its one saver) picks up what `tritone login` wrote in another process, and its older cookies aren't saved over them. `save_if_dirty(secrets, now_ms)` (`src/discord/transport.rs:212`) writes only when changed, at most every 5 s, and a minute after a failed write; session cookies aren't persisted, like a browser restart. `spawn_saver(secrets, timer)` checks every second.

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
rsa 0.9 (OAEP-SHA256, `rsa::rand_core::OsRng`), sha2, futures-util, serde, serde_json (`preserve_order`), schemars, thiserror, async-trait, wreq 0.16.3, wreq-util 0.2.1, tokio-tungstenite 0.30, cookie_store 0.22, url, base64, sys-locale, iana-time-zone. Building wreq compiles BoringSSL: cmake, a C/C++ compiler and libclang (see README).

## Gotchas & invariants
- Never log tokens, cookies or message text from here. `HttpDiscord` network errors are reduced to "timed out" / "could not connect" / "request failed" (wreq's text names the URL).
- A halt in `HttpDiscord` lasts until the user clears it: `clear_halt()` (`src/discord/http.rs:522`, the `DiscordApi` override `SendQueue::resume` calls together with `LiveState::clear_halt`) lifts a `Captcha`, `Locked` or `Blocked` halt, never `NeedsLogin`; a dead token needs a new login, which builds a new client (`rebuild_api`). Tests assert the halted call count stays at one request, including a send already waiting for its turn when the read before it halted.
- Header order for `Authorization`/`Content-Type` within Chrome's set is a best guess; compare with a browser capture on the first live run.
- `refresh` is the only code here that reaches discord.com without a token: the two unauthenticated GETs above. Tests never touch the network: they point `Endpoints` at wiremock.
- `GuildUpsert` replaces the guild's whole channel set: the gateway must include the guild's joined threads in `channels`, or they're dropped. Use it for GUILD_CREATE only; GUILD_UPDATE is `GuildUpdate`.
- The Keychain stores at most ~4 KB per value (`security -i` line limit); the cookie jar is deflated to stay far below it.

## Testing
`cargo test discord`.
