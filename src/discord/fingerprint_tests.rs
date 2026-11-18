use base64::Engine;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::secrets::fake::MemSecrets;

fn fp() -> Fingerprint {
    let store = Store::open_in_memory().unwrap();
    Fingerprint::load(&store, &MemSecrets::default()).unwrap()
}

fn header<'a>(h: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
    h.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
}

#[test]
fn super_properties_has_required_fields() {
    let f = fp();
    let headers = f.rest_headers(false);
    let raw = header(&headers, "X-Super-Properties").expect("super-properties header");
    let json = base64::engine::general_purpose::STANDARD.decode(raw).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
    for field in [
        "os",
        "browser",
        "browser_user_agent",
        "browser_version",
        "client_build_number",
        "release_channel",
        "system_locale",
    ] {
        assert!(!v[field].is_null(), "missing {field}: {v}");
    }
    assert_eq!(v["browser"], "Chrome");
    assert_eq!(v["release_channel"], "stable");
    assert_eq!(v["client_build_number"], FALLBACK_BUILD);
    assert!(v.as_object().unwrap().contains_key("client_event_source"));
    assert!(v["client_event_source"].is_null());
    assert!(v.get("launch_signature").is_none());
    assert!(v.get("has_client_mods").is_none());
    assert_eq!(v["browser_user_agent"], header(&headers, "User-Agent").unwrap(), "UA must match the header");
    // Compact JSON, in the browser's field order.
    assert_eq!(std::str::from_utf8(&json).unwrap(), serde_json::to_string(&f.super_properties()).unwrap());
    assert!(std::str::from_utf8(&json).unwrap().starts_with("{\"os\":"));
}

#[test]
fn identify_properties_add_installation_id_when_known() {
    let mut f = fp();
    assert!(f.identify_properties().get("installation_id").is_none());
    f.installation_id = Some("1.abc".into());
    let v = f.identify_properties();
    assert_eq!(v["installation_id"], "1.abc");
    assert_eq!(v["browser"], "Chrome");
    let h = f.rest_headers(false);
    assert_eq!(header(&h, "X-Installation-ID"), Some("1.abc"));
}

#[test]
fn rest_headers_origin_only_on_write() {
    let f = fp();
    let read = f.rest_headers(false);
    let write = f.rest_headers(true);
    assert_eq!(header(&read, "Origin"), None);
    assert_eq!(header(&write, "Origin"), Some("https://discord.com"));
    for h in [&read, &write] {
        assert_eq!(header(h, "Referer"), Some("https://discord.com/channels/@me"));
        assert_eq!(header(h, "Sec-Fetch-Mode"), Some("cors"));
        assert_eq!(header(h, "Sec-Fetch-Site"), Some("same-origin"));
        assert_eq!(header(h, "Sec-Fetch-Dest"), Some("empty"));
        assert_eq!(header(h, "X-Discord-Locale"), Some(f.locale.as_str()));
        assert_eq!(header(h, "X-Discord-Timezone"), Some(f.timezone.as_str()));
        assert!(header(h, "Accept-Language").is_some());
        assert!(header(h, "Authorization").is_none(), "the REST client adds the token, not the fingerprint");
    }
}

#[test]
fn ws_headers_keep_capitalized_origin() {
    let f = fp();
    let h = f.ws_headers();
    let names: Vec<&str> = h.iter().map(|(n, _)| *n).collect();
    assert!(names.contains(&"Origin"), "{names:?}");
    assert!(!names.contains(&"origin"));
    assert_eq!(header(&h, "Origin"), Some("https://discord.com"));
    assert_eq!(header(&h, "User-Agent"), Some(f.user_agent.as_str()));
    assert_eq!(header(&h, "Cache-Control"), Some("no-cache"));
    assert_eq!(header(&h, "Pragma"), Some("no-cache"));
    assert!(header(&h, "Accept-Language").is_some());
}

#[test]
fn locale_maps_to_discord_locales() {
    assert_eq!(discord_locale("es-CL"), "es-419");
    assert_eq!(discord_locale("es_ES"), "es-ES");
    assert_eq!(discord_locale("en-AU"), "en-US");
    assert_eq!(discord_locale("en-GB"), "en-GB");
    assert_eq!(discord_locale("pt-PT"), "pt-BR");
    assert_eq!(discord_locale("zh-HK"), "zh-TW");
    assert_eq!(discord_locale("zh-Hans-CN"), "zh-CN");
    assert_eq!(discord_locale("de-AT"), "de");
    assert_eq!(discord_locale("sv"), "sv-SE");
    assert_eq!(discord_locale("xx"), "en-US");
    assert_eq!(accept_language("es-419"), "es-419,es;q=0.9");
    assert_eq!(accept_language("de"), "de");
}

#[test]
fn load_reads_cached_build_and_installation() {
    let store = Store::open_in_memory().unwrap();
    store.kv_set(KV_BUILD, r#"{"build":123456,"at":5}"#).unwrap();
    store.kv_set(KV_INSTALLATION, "1.abc").unwrap();
    let f = Fingerprint::load(&store, &MemSecrets::default()).unwrap();
    assert_eq!(f.build_number, 123456);
    assert_eq!(f.installation_id.as_deref(), Some("1.abc"));
    // A broken cache falls back instead of failing.
    store.kv_set(KV_BUILD, "garbage").unwrap();
    assert_eq!(Fingerprint::load(&store, &MemSecrets::default()).unwrap().build_number, FALLBACK_BUILD);
}

const APP_HTML: &str = r#"<html><head><link as="script" href="/assets/web.abc123.js" rel="preload"><link as="script" href="/assets/sentry.0f1e2d.js" rel="preload"></head><body><script defer src="/assets/sentry.0f1e2d.js"></script></body></html>"#;
const SENTRY_JS: &str = r#"x.NA("release","abc"),x.NA("buildNumber","654321"),x.NA("builtAt",String(1))"#;

#[test]
fn sentry_parsers() {
    assert_eq!(find_sentry_asset(APP_HTML), Some("/assets/sentry.0f1e2d.js"));
    assert_eq!(find_sentry_asset("<html></html>"), None);
    assert_eq!(parse_build_number(SENTRY_JS), Some(654321));
    assert_eq!(parse_build_number(r#"buildNumber","""#), None);
    assert_eq!(parse_build_number("nothing"), None);
}

async fn sentry_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/app"))
        .respond_with(ResponseTemplate::new(200).set_body_string(APP_HTML))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/assets/sentry.0f1e2d.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SENTRY_JS))
        .mount(&server)
        .await;
    server
}

fn plain_client() -> wreq::Client {
    wreq::Client::builder().build().unwrap()
}

#[tokio::test]
async fn build_number_parses_sentry_asset() {
    let server = sentry_server().await;
    assert_eq!(refresh_build_number_from(&plain_client(), &server.uri()).await, Some(654321));
}

#[tokio::test]
async fn build_number_refresh_failure_is_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/app")).respond_with(ResponseTemplate::new(503)).mount(&server).await;
    assert_eq!(refresh_build_number_from(&plain_client(), &server.uri()).await, None);
}

#[tokio::test]
async fn installation_id_comes_from_apex_experiments() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v9/apex/experiments"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"installation":"1.abc_def","assignments":{}}"#))
        .mount(&server)
        .await;
    let api = format!("{}/api/v9", server.uri());
    assert_eq!(fetch_installation_id_from(&plain_client(), &api).await.as_deref(), Some("1.abc_def"));
    let req = &server.received_requests().await.unwrap()[0];
    assert_eq!(req.url.query(), Some("surface=2"));
    assert!(req.headers.get("authorization").is_none(), "unauthenticated");
}

#[tokio::test]
async fn refresh_caches_build_and_installation_and_skips_when_fresh() {
    let server = sentry_server().await;
    Mock::given(method("GET"))
        .and(path("/api/v9/apex/experiments"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"installation":"1.abc"}"#))
        .mount(&server)
        .await;
    let store = Store::open_in_memory().unwrap();
    let mut f = Fingerprint::load(&store, &MemSecrets::default()).unwrap();
    let ends = Endpoints { web: server.uri(), api: format!("{}/api/v9", server.uri()) };
    let now = 1_800_000_000_000;
    f.refresh_with(&plain_client(), &store, now, &ends).await;
    assert_eq!(f.build_number, 654321);
    assert_eq!(f.installation_id.as_deref(), Some("1.abc"));
    assert_eq!(store.kv_get(KV_INSTALLATION).unwrap().as_deref(), Some("1.abc"));
    let reloaded = Fingerprint::load(&store, &MemSecrets::default()).unwrap();
    assert_eq!(reloaded.build_number, 654321);

    let calls = server.received_requests().await.unwrap().len();
    f.refresh_with(&plain_client(), &store, now + 60_000, &ends).await;
    assert_eq!(server.received_requests().await.unwrap().len(), calls, "fresh cache: no requests");
    f.refresh_with(&plain_client(), &store, now + BUILD_MAX_AGE_MS + 1, &ends).await;
    assert_eq!(server.received_requests().await.unwrap().len(), calls + 2, "stale build: app + sentry again");
}
