//! The network stack: a wreq client that emulates Chrome (TLS and HTTP/2
//! fingerprint), a persistent cookie jar, and WebSocket upgrades done through
//! that same client.
//!
//! Plain rustls clients get a CAPTCHA at login, so every Discord call goes
//! through `http_client`. The emulation's own default headers are off: they are
//! a page navigation's (`upgrade-insecure-requests`, `sec-fetch-dest:
//! document`), while tritone makes API calls, so `Fingerprint::rest_headers`
//! supplies the full set, with the same UA and `sec-ch-ua` as the profile.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::{client::generate_key, derive_accept_key};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use wreq::cookie::Cookies;
use wreq::header::{HeaderValue, OrigHeaderMap};
use wreq::{Uri, Version};
use wreq_util::{Emulation, Platform, Profile};

use crate::domain::clock::Timer;
use crate::secrets::{ACCOUNT_COOKIES, SecretStore};

/// The newest Chrome profile in wreq-util 0.2.1.
pub const CHROME_PROFILE: Profile = Profile::Chrome149;
/// READY grows with the account; tungstenite's 16 MiB default is too small.
pub const WS_MAX_SIZE: usize = 256 << 20;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The jar is written to the keyring at most this often.
const SAVE_EVERY_MS: i64 = 5_000;
/// After a failed write, wait this long before trying again.
const SAVE_RETRY_MS: i64 = 60_000;

/// What the Chrome profile claims to be. These mirror wreq-util 0.2.1's
/// Chrome149 profile (`src/emulate/profile/chrome.rs`, `v149`); a test checks
/// they still match what the profile sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeIdentity {
    pub user_agent: &'static str,
    pub sec_ch_ua: &'static str,
    pub sec_ch_ua_platform: &'static str,
    pub browser_version: &'static str,
    /// Super-properties `os` and `os_version`, as Discord web derives them from the UA.
    pub os: &'static str,
    pub os_version: &'static str,
}

const SEC_CH_UA: &str = r#""Google Chrome";v="149", "Chromium";v="149", "Not)A;Brand";v="24""#;

/// The wreq-util platform for this OS. tritone runs on macOS and Linux only.
pub fn platform() -> Platform {
    if cfg!(target_os = "macos") { Platform::MacOS } else { Platform::Linux }
}

pub fn chrome_identity() -> ChromeIdentity {
    if cfg!(target_os = "macos") {
        ChromeIdentity {
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36",
            sec_ch_ua: SEC_CH_UA,
            sec_ch_ua_platform: "\"macOS\"",
            browser_version: "149.0.0.0",
            os: "Mac OS X",
            os_version: "10.15.7",
        }
    } else {
        ChromeIdentity {
            user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36",
            sec_ch_ua: SEC_CH_UA,
            sec_ch_ua_platform: "\"Linux\"",
            browser_version: "149.0.0.0",
            os: "Linux",
            os_version: "",
        }
    }
}

/// The one HTTP client for Discord: Chrome emulation, the cookie jar, 30 s timeout.
pub fn http_client(cookies: Arc<CookieJar>) -> Result<wreq::Client> {
    let emulation = Emulation::builder().profile(CHROME_PROFILE).platform(platform()).headers(false).build();
    Ok(wreq::Client::builder().emulation(emulation).cookie_provider(cookies).timeout(REQUEST_TIMEOUT).build()?)
}

pub type WsStream = WebSocketStream<wreq::Upgraded>;

/// Chrome's header order on a WebSocket upgrade. Headers not listed go last.
const WS_HEADER_ORDER: &[&str] = &[
    "Host",
    "Connection",
    "Pragma",
    "Cache-Control",
    "User-Agent",
    "Upgrade",
    "Origin",
    "Sec-WebSocket-Version",
    "Accept-Encoding",
    "Accept-Language",
    "Sec-WebSocket-Key",
];

/// Opens a WebSocket: the HTTP/1.1 upgrade goes through `client` (Chrome TLS,
/// cookies), with header names sent exactly as given (Cloudflare rejects a
/// lowercase `Origin`), then tokio-tungstenite handles the frames.
pub async fn ws_connect(client: &wreq::Client, url: &str, headers: &[(&str, String)]) -> Result<WsStream> {
    let http_url = match url.split_once("://") {
        Some(("wss", rest)) => format!("https://{rest}"),
        Some(("ws", rest)) => format!("http://{rest}"),
        _ => bail!("not a WebSocket URL"),
    };
    let key = generate_key();
    let mut all: Vec<(String, String)> = headers.iter().map(|(n, v)| (n.to_string(), v.clone())).collect();
    all.push(("Connection".into(), "Upgrade".into()));
    all.push(("Upgrade".into(), "websocket".into()));
    all.push(("Sec-WebSocket-Version".into(), "13".into()));
    all.push(("Sec-WebSocket-Key".into(), key.clone()));
    let rank = |n: &str| WS_HEADER_ORDER.iter().position(|o| o.eq_ignore_ascii_case(n)).unwrap_or(usize::MAX);
    all.sort_by_key(|(n, _)| rank(n));

    let mut orig = OrigHeaderMap::new();
    orig.insert("Host");
    let mut req = client.get(http_url).version(Version::HTTP_11);
    for (name, value) in all {
        orig.insert(name.clone());
        req = req.header(name, value);
    }
    let resp = req.orig_headers(orig).send().await.context("WebSocket upgrade request failed")?;
    let status = resp.status();
    if status != wreq::StatusCode::SWITCHING_PROTOCOLS {
        bail!("WebSocket upgrade refused: HTTP {}", status.as_u16());
    }
    let accept = resp.headers().get("sec-websocket-accept").and_then(|v| v.to_str().ok());
    if accept != Some(derive_accept_key(key.as_bytes()).as_str()) {
        bail!("WebSocket upgrade: bad Sec-WebSocket-Accept");
    }
    let io = resp.upgrade().await.context("WebSocket upgrade failed")?;
    let config = WebSocketConfig::default().max_message_size(Some(WS_MAX_SIZE)).max_frame_size(Some(WS_MAX_SIZE));
    Ok(WebSocketStream::from_raw_socket(io, Role::Client, Some(config)).await)
}

/// The persistent cookie jar, so tritone stays the same "device" across
/// restarts. Keeps discord.com cookies only. Saved to the keyring (account
/// `cookies`) as base64 of deflated cookie_store JSON, persistent cookies
/// only. Deflate keeps it well under the Keychain's ~4 KB line limit.
pub struct CookieJar {
    store: Mutex<cookie_store::CookieStore>,
    dirty: AtomicBool,
    /// Earliest time of the next write: debounce after a save, back-off after a failure.
    next_save_ms: AtomicI64,
}

fn is_discord(uri: &Uri) -> bool {
    uri.host().is_some_and(|h| h == "discord.com" || h.ends_with(".discord.com"))
}

fn to_url(uri: &Uri) -> Option<url::Url> {
    url::Url::parse(&uri.to_string()).ok()
}

impl CookieJar {
    pub fn empty() -> CookieJar {
        CookieJar::from_store(cookie_store::CookieStore::default())
    }

    fn from_store(store: cookie_store::CookieStore) -> CookieJar {
        CookieJar { store: Mutex::new(store), dirty: AtomicBool::new(false), next_save_ms: AtomicI64::new(i64::MIN) }
    }

    fn lock(&self) -> MutexGuard<'_, cookie_store::CookieStore> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The saved jar, or an empty one when none is saved or it can't be read.
    pub async fn load(secrets: &dyn SecretStore) -> Result<CookieJar> {
        let Some(saved) = secrets.get(ACCOUNT_COOKIES).await? else { return Ok(CookieJar::empty()) };
        let parsed = base64::engine::general_purpose::STANDARD.decode(saved.trim()).ok().and_then(|packed| {
            cookie_store::serde::json::load(std::io::BufReader::new(DeflateDecoder::new(&packed[..]))).ok()
        });
        match parsed {
            Some(store) => Ok(CookieJar::from_store(store)),
            None => {
                tracing::warn!("saved cookie jar is unreadable; starting empty");
                Ok(CookieJar::empty())
            }
        }
    }

    /// Replaces the jar's contents with the keyring's, in place, so the
    /// clients and the saver holding this jar see what `tritone login` just
    /// wrote (the login runs in another process). Clears `dirty`: the
    /// daemon's older cookies must not be saved over the login's.
    pub async fn reload(&self, secrets: &dyn SecretStore) -> Result<()> {
        let fresh = CookieJar::load(secrets).await?;
        let store = std::mem::take(&mut *fresh.lock());
        *self.lock() = store;
        self.dirty.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Changed since the last save.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    /// Writes the jar when it changed and the last save is at least 5 s old
    /// (a minute after a failed one). Returns whether it wrote.
    pub async fn save_if_dirty(&self, secrets: &dyn SecretStore, now_ms: i64) -> Result<bool> {
        if !self.is_dirty() || now_ms < self.next_save_ms.load(Ordering::SeqCst) {
            return Ok(false);
        }
        self.dirty.store(false, Ordering::SeqCst);
        match self.write(secrets).await {
            Ok(()) => {
                self.next_save_ms.store(now_ms.saturating_add(SAVE_EVERY_MS), Ordering::SeqCst);
                Ok(true)
            }
            Err(e) => {
                self.dirty.store(true, Ordering::SeqCst);
                self.next_save_ms.store(now_ms.saturating_add(SAVE_RETRY_MS), Ordering::SeqCst);
                Err(e)
            }
        }
    }

    async fn write(&self, secrets: &dyn SecretStore) -> Result<()> {
        let mut deflate = DeflateEncoder::new(Vec::new(), Compression::best());
        cookie_store::serde::json::save(&self.lock(), &mut deflate)
            .map_err(|e| anyhow::anyhow!("serializing the cookie jar failed: {e}"))?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(deflate.finish()?);
        secrets.set(ACCOUNT_COOKIES, &encoded).await
    }

    /// Saves in the background, checking every second.
    pub fn spawn_saver(
        self: Arc<Self>,
        secrets: Arc<dyn SecretStore>,
        timer: Arc<dyn Timer>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Err(e) = self.save_if_dirty(secrets.as_ref(), timer.now_ms()).await {
                    tracing::warn!("saving cookies failed: {e}");
                }
            }
        })
    }
}

impl wreq::cookie::CookieStore for CookieJar {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, uri: &Uri) {
        if !is_discord(uri) {
            return;
        }
        let Some(url) = to_url(uri) else { return };
        let mut store = self.lock();
        for h in cookie_headers {
            let Ok(s) = h.to_str() else { continue };
            let Ok(raw) = cookie_store::RawCookie::parse(s.to_string()) else { continue };
            if store.insert_raw(&raw, &url).is_ok() {
                self.dirty.store(true, Ordering::SeqCst);
            }
        }
    }

    fn cookies(&self, uri: &Uri, version: Version) -> Cookies {
        if !is_discord(uri) {
            return Cookies::Empty;
        }
        let Some(url) = to_url(uri) else { return Cookies::Empty };
        let pairs: Vec<String> = self.lock().get_request_values(&url).map(|(n, v)| format!("{n}={v}")).collect();
        if pairs.is_empty() {
            return Cookies::Empty;
        }
        if version == Version::HTTP_11 || version == Version::HTTP_10 {
            match HeaderValue::from_str(&pairs.join("; ")) {
                Ok(v) => Cookies::Compressed(v),
                Err(_) => Cookies::Empty,
            }
        } else {
            Cookies::Uncompressed(pairs.iter().filter_map(|p| HeaderValue::from_str(p).ok()).collect())
        }
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
