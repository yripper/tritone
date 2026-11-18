//! What tritone tells Discord about itself: Discord web in Chrome. The
//! super-properties, the REST and WebSocket headers, the build number and the
//! installation id.
//!
//! Field set: https://docs.discord.food/reference#client-properties. Never
//! sends `launch_signature` or `has_client_mods`.

use std::time::Duration;

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::transport::chrome_identity;
use crate::secrets::SecretStore;
use crate::store::Store;

/// The web client's build number, read live from discord.com on 2026-11-18.
/// Used until a live refresh succeeds.
pub const FALLBACK_BUILD: u64 = 675_174;
/// Refresh the build number at start and once it's older than this.
pub const BUILD_MAX_AGE_MS: i64 = 24 * 3_600_000;
pub const KV_BUILD: &str = "build_number";
pub const KV_INSTALLATION: &str = "installation_id";
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(5);
/// Chrome's `Accept` for a page navigation.
const DOCUMENT_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7";

/// Where the scrape and the installation id come from. Tests point it at a mock.
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// `https://discord.com`
    pub web: String,
    /// `https://discord.com/api/v9`
    pub api: String,
}

impl Endpoints {
    pub fn discord() -> Endpoints {
        Endpoints { web: "https://discord.com".into(), api: "https://discord.com/api/v9".into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    pub user_agent: String,
    pub browser_version: String,
    pub os: String,
    pub os_version: String,
    /// A Discord locale, e.g. `en-US` or `es-419`.
    pub locale: String,
    /// IANA zone, e.g. `America/New_York`; `UTC` when unknown.
    pub timezone: String,
    pub build_number: u64,
    pub installation_id: Option<String>,
    /// `sec-ch-ua` and `sec-ch-ua-platform`, from the same Chrome profile as the UA.
    pub sec_ch_ua: String,
    pub sec_ch_ua_platform: String,
}

/// The cached build number in `kv`.
#[derive(Serialize, Deserialize)]
struct CachedBuild {
    build: u64,
    /// Unix ms of the scrape.
    at: i64,
}

fn cached_build(store: &Store) -> Option<CachedBuild> {
    serde_json::from_str(&store.kv_get(KV_BUILD).ok()??).ok()
}

/// Discord's supported locales (https://docs.discord.food/reference#locales).
const LOCALES: &[&str] = &[
    "id", "da", "de", "en-GB", "en-US", "es-ES", "es-419", "fr", "hr", "it", "lt", "hu", "nl", "no", "pl", "pt-BR",
    "ro", "fi", "sv-SE", "vi", "tr", "cs", "el", "bg", "ru", "uk", "hi", "th", "zh-CN", "ja", "zh-TW", "ko",
];

/// The closest Discord locale to an OS locale such as `es_CL` or `zh-Hant-HK`.
pub fn discord_locale(os_locale: &str) -> &'static str {
    let tag = os_locale.split('.').next().unwrap_or("").replace('_', "-");
    if let Some(exact) = LOCALES.iter().find(|l| l.eq_ignore_ascii_case(&tag)) {
        return exact;
    }
    let mut parts = tag.split('-');
    let lang = parts.next().unwrap_or("").to_ascii_lowercase();
    let rest: Vec<String> = parts.map(|p| p.to_ascii_uppercase()).collect();
    let has = |s: &str| rest.iter().any(|p| p == s);
    match lang.as_str() {
        "es" => {
            if has("ES") {
                "es-ES"
            } else {
                "es-419"
            }
        }
        "en" => "en-US",
        "pt" => "pt-BR",
        "sv" => "sv-SE",
        "nb" | "nn" => "no",
        "zh" => {
            if has("TW") || has("HK") || has("MO") || has("HANT") {
                "zh-TW"
            } else {
                "zh-CN"
            }
        }
        _ => LOCALES.iter().find(|l| **l == lang).copied().unwrap_or("en-US"),
    }
}

/// `Accept-Language` for a Discord locale, the way Chrome sends it.
pub fn accept_language(locale: &str) -> String {
    match locale.split_once('-') {
        Some((lang, _)) => format!("{locale},{lang};q=0.9"),
        None => locale.to_string(),
    }
}

/// The super-properties in the web client's field order (needs serde_json's
/// `preserve_order`).
#[derive(Serialize)]
struct SuperProperties<'a> {
    os: &'a str,
    browser: &'static str,
    device: &'static str,
    system_locale: &'a str,
    browser_user_agent: &'a str,
    browser_version: &'a str,
    os_version: &'a str,
    referrer: &'static str,
    referring_domain: &'static str,
    referrer_current: &'static str,
    referring_domain_current: &'static str,
    release_channel: &'static str,
    client_build_number: u64,
    client_event_source: Option<()>,
}

impl Fingerprint {
    /// The fingerprint for this machine: the Chrome profile's identity, the
    /// OS locale and time zone, and the cached build number and installation
    /// id (the fallback build when none is cached). Nothing here is secret, so
    /// `_secrets` is unused today; it stays for callers that pass the keyring.
    pub fn load(store: &Store, _secrets: &dyn SecretStore) -> Result<Fingerprint> {
        Ok(Fingerprint {
            build_number: cached_build(store).map_or(FALLBACK_BUILD, |c| c.build),
            installation_id: store.kv_get(KV_INSTALLATION)?,
            ..Fingerprint::for_this_machine()
        })
    }

    /// This machine's identity with the fallback build and no installation id.
    pub fn for_this_machine() -> Fingerprint {
        let id = chrome_identity();
        let os_locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".into());
        Fingerprint {
            user_agent: id.user_agent.into(),
            browser_version: id.browser_version.into(),
            os: id.os.into(),
            os_version: id.os_version.into(),
            locale: discord_locale(&os_locale).into(),
            timezone: iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".into()),
            build_number: FALLBACK_BUILD,
            installation_id: None,
            sec_ch_ua: id.sec_ch_ua.into(),
            sec_ch_ua_platform: id.sec_ch_ua_platform.into(),
        }
    }

    pub fn super_properties(&self) -> Value {
        let props = SuperProperties {
            os: &self.os,
            browser: "Chrome",
            device: "",
            system_locale: &self.locale,
            browser_user_agent: &self.user_agent,
            browser_version: &self.browser_version,
            os_version: &self.os_version,
            referrer: "",
            referring_domain: "",
            referrer_current: "",
            referring_domain_current: "",
            release_channel: "stable",
            client_build_number: self.build_number,
            client_event_source: None,
        };
        serde_json::to_value(props).unwrap_or(Value::Null)
    }

    /// The identify `properties`: the super-properties plus `installation_id` when known.
    pub fn identify_properties(&self) -> Value {
        let mut v = self.super_properties();
        if let (Some(map), Some(id)) = (v.as_object_mut(), &self.installation_id) {
            map.insert("installation_id".into(), json!(id));
        }
        v
    }

    fn super_properties_header(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.super_properties().to_string())
    }

    /// Headers for every REST call, in Chrome's order. `Origin` goes only on
    /// writes. The REST client adds `Authorization`.
    pub fn rest_headers(&self, write: bool) -> Vec<(&'static str, String)> {
        let mut h = vec![
            ("Sec-Ch-Ua-Platform", self.sec_ch_ua_platform.clone()),
            ("X-Discord-Locale", self.locale.clone()),
            ("X-Discord-Timezone", self.timezone.clone()),
            ("X-Super-Properties", self.super_properties_header()),
            ("Sec-Ch-Ua", self.sec_ch_ua.clone()),
            ("Sec-Ch-Ua-Mobile", "?0".into()),
            ("User-Agent", self.user_agent.clone()),
            ("Accept", "*/*".into()),
        ];
        if let Some(id) = &self.installation_id {
            h.push(("X-Installation-ID", id.clone()));
        }
        if write {
            h.push(("Origin", "https://discord.com".into()));
        }
        h.extend([
            ("Sec-Fetch-Site", "same-origin".into()),
            ("Sec-Fetch-Mode", "cors".into()),
            ("Sec-Fetch-Dest", "empty".into()),
            ("Referer", "https://discord.com/channels/@me".into()),
            ("Accept-Encoding", "gzip, deflate, br, zstd".into()),
            ("Accept-Language", accept_language(&self.locale)),
            ("Priority", "u=1, i".into()),
        ]);
        h
    }

    /// Chrome opening `https://discord.com/app` in a new tab.
    fn document_headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Sec-Ch-Ua", self.sec_ch_ua.clone()),
            ("Sec-Ch-Ua-Mobile", "?0".into()),
            ("Sec-Ch-Ua-Platform", self.sec_ch_ua_platform.clone()),
            ("Upgrade-Insecure-Requests", "1".into()),
            ("User-Agent", self.user_agent.clone()),
            ("Accept", DOCUMENT_ACCEPT.into()),
            ("Sec-Fetch-Site", "none".into()),
            ("Sec-Fetch-Mode", "navigate".into()),
            ("Sec-Fetch-User", "?1".into()),
            ("Sec-Fetch-Dest", "document".into()),
            ("Accept-Encoding", "gzip, deflate, br, zstd".into()),
            ("Accept-Language", accept_language(&self.locale)),
            ("Priority", "u=0, i".into()),
        ]
    }

    /// The app page loading one of its scripts.
    fn script_headers(&self, referer: String) -> Vec<(&'static str, String)> {
        vec![
            ("Sec-Ch-Ua-Platform", self.sec_ch_ua_platform.clone()),
            ("User-Agent", self.user_agent.clone()),
            ("Sec-Ch-Ua", self.sec_ch_ua.clone()),
            ("Sec-Ch-Ua-Mobile", "?0".into()),
            ("Accept", "*/*".into()),
            ("Sec-Fetch-Site", "same-origin".into()),
            ("Sec-Fetch-Mode", "no-cors".into()),
            ("Sec-Fetch-Dest", "script".into()),
            ("Referer", referer),
            ("Accept-Encoding", "gzip, deflate, br, zstd".into()),
            ("Accept-Language", accept_language(&self.locale)),
        ]
    }

    /// Headers for the gateway's WebSocket upgrade. Names keep their casing:
    /// Cloudflare answers 403 to a lowercase `Origin` on HTTP/1.1.
    pub fn ws_headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Pragma", "no-cache".into()),
            ("Cache-Control", "no-cache".into()),
            ("User-Agent", self.user_agent.clone()),
            ("Origin", "https://discord.com".into()),
            ("Accept-Language", accept_language(&self.locale)),
        ]
    }

    /// Refreshes from discord.com: the build number when the cache is older
    /// than 24 h (or missing), and the installation id when unknown. Failures
    /// keep the current values.
    pub async fn refresh(&mut self, client: &wreq::Client, store: &Store, now_ms: i64) {
        self.refresh_with(client, store, now_ms, &Endpoints::discord()).await
    }

    pub async fn refresh_with(&mut self, client: &wreq::Client, store: &Store, now_ms: i64, ends: &Endpoints) {
        let stale = cached_build(store).is_none_or(|c| now_ms - c.at > BUILD_MAX_AGE_MS);
        if stale {
            if let Some(build) = refresh_build_number_from(client, self, &ends.web).await {
                self.build_number = build;
                let cached = serde_json::to_string(&CachedBuild { build, at: now_ms }).unwrap_or_default();
                if let Err(e) = store.kv_set(KV_BUILD, &cached) {
                    tracing::warn!("caching the build number failed: {e}");
                }
            } else {
                tracing::warn!("build number refresh failed; keeping {}", self.build_number);
            }
        }
        if self.installation_id.is_none()
            && let Some(id) = fetch_installation_id_from(client, self, &ends.api).await
        {
            if let Err(e) = store.kv_set(KV_INSTALLATION, &id) {
                tracing::warn!("saving the installation id failed: {e}");
            }
            self.installation_id = Some(id);
        }
    }
}

/// The `/assets/sentry*.js` path in the web app's HTML.
pub fn find_sentry_asset(html: &str) -> Option<&str> {
    let start = html.find("/assets/sentry")?;
    let rest = &html[start..];
    let end = rest.find(|c: char| c == '"' || c == '\'' || c.is_whitespace() || c == '>')?;
    let path = &rest[..end];
    path.ends_with(".js").then_some(path)
}

/// The digits after `buildNumber","` in the sentry asset.
pub fn parse_build_number(js: &str) -> Option<u64> {
    const MARK: &str = r#"buildNumber",""#;
    let rest = &js[js.find(MARK)? + MARK.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// A GET with the given browser headers (the client's emulation sends none).
async fn get_text(client: &wreq::Client, url: &str, headers: Vec<(&'static str, String)>) -> Option<String> {
    let mut req = client.get(url).timeout(SCRAPE_TIMEOUT);
    for (name, value) in headers {
        req = req.header(name, value);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().await.ok()
}

/// The live build number from discord.com: the web app's HTML names its
/// sentry asset, which carries the build number. Sent with Chrome's page and
/// script headers. 5 s timeouts.
pub async fn refresh_build_number(client: &wreq::Client) -> Option<u64> {
    refresh_build_number_from(client, &Fingerprint::for_this_machine(), &Endpoints::discord().web).await
}

pub async fn refresh_build_number_from(client: &wreq::Client, fp: &Fingerprint, web_base: &str) -> Option<u64> {
    let page = format!("{web_base}/app");
    let html = get_text(client, &page, fp.document_headers()).await?;
    let asset = find_sentry_asset(&html)?.to_string();
    parse_build_number(&get_text(client, &format!("{web_base}{asset}"), fp.script_headers(page)).await?)
}

/// An installation id from the unauthenticated `GET /apex/experiments?surface=2`
/// (https://docs.discord.food/topics/experiments).
pub async fn fetch_installation_id(client: &wreq::Client) -> Option<String> {
    fetch_installation_id_from(client, &Fingerprint::for_this_machine(), &Endpoints::discord().api).await
}

pub async fn fetch_installation_id_from(client: &wreq::Client, fp: &Fingerprint, api_base: &str) -> Option<String> {
    let url = format!("{api_base}/apex/experiments?surface=2");
    let body = get_text(client, &url, fp.rest_headers(false)).await?;
    let v: Value = serde_json::from_str(&body).ok()?;
    v.get("installation")?.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

#[cfg(test)]
#[path = "fingerprint_tests.rs"]
mod tests;
