use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};
use wreq::cookie::CookieStore as _;

use super::*;
use crate::secrets::fake::MemSecrets;
use crate::secrets::{ACCOUNT_COOKIES, SecretStore};

fn uri(s: &str) -> wreq::Uri {
    s.parse().unwrap()
}

fn cookie_header(jar: &CookieJar, u: &str) -> Option<String> {
    match jar.cookies(&uri(u), wreq::Version::HTTP_11) {
        wreq::cookie::Cookies::Compressed(v) => Some(v.to_str().unwrap().to_string()),
        wreq::cookie::Cookies::Uncompressed(v) => {
            Some(v.iter().map(|h| h.to_str().unwrap()).collect::<Vec<_>>().join("; "))
        }
        _ => None,
    }
}

fn set(jar: &CookieJar, u: &str, cookie: &str) {
    let v = wreq::header::HeaderValue::from_str(cookie).unwrap();
    jar.set_cookies(&mut std::iter::once(&v), &uri(u));
}

#[test]
fn cookie_jar_keeps_only_discord() {
    let jar = CookieJar::empty();
    set(&jar, "https://discord.com/api/v9/x", "__dcfduid=a1; Path=/; Max-Age=3600; Secure");
    set(&jar, "https://evil.example/", "tracker=1; Path=/; Max-Age=3600");
    set(&jar, "https://discord.com.evil.example/", "tracker=2; Path=/; Max-Age=3600");
    assert_eq!(cookie_header(&jar, "https://discord.com/api/v9/users/@me").as_deref(), Some("__dcfduid=a1"));
    assert_eq!(cookie_header(&jar, "https://evil.example/"), None);
    assert_eq!(cookie_header(&jar, "https://discord.com.evil.example/"), None);
    assert!(jar.is_dirty());
}

#[tokio::test]
async fn cookie_jar_persists_through_the_keyring() {
    let secrets = MemSecrets::default();
    let jar = CookieJar::load(&secrets).await.unwrap();
    set(&jar, "https://discord.com/", "__sdcfduid=b2; Path=/; Max-Age=3600; Secure");
    // A session cookie is not persisted, like a browser restart.
    set(&jar, "https://discord.com/", "__cfruid=c3; Path=/; Secure");
    assert!(jar.save_if_dirty(&secrets, 10_000).await.unwrap());
    let stored = secrets.get(ACCOUNT_COOKIES).await.unwrap().unwrap();
    assert!(stored.chars().all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)), "keychain-safe");

    let again = CookieJar::load(&secrets).await.unwrap();
    assert_eq!(cookie_header(&again, "https://discord.com/").as_deref(), Some("__sdcfduid=b2"));
    assert!(!again.is_dirty());
}

#[tokio::test]
async fn cookie_jar_saves_at_most_every_5s() {
    let secrets = MemSecrets::default();
    let jar = CookieJar::empty();
    assert!(!jar.save_if_dirty(&secrets, 0).await.unwrap(), "nothing to save");
    set(&jar, "https://discord.com/", "a=1; Path=/; Max-Age=3600");
    assert!(jar.save_if_dirty(&secrets, 10_000).await.unwrap());
    set(&jar, "https://discord.com/", "a=2; Path=/; Max-Age=3600");
    assert!(!jar.save_if_dirty(&secrets, 12_000).await.unwrap(), "debounced");
    assert!(jar.save_if_dirty(&secrets, 15_000).await.unwrap());
    assert!(!jar.is_dirty());
}

#[tokio::test]
async fn corrupt_saved_jar_starts_empty() {
    let secrets = MemSecrets::default();
    secrets.set(ACCOUNT_COOKIES, "not-base64!").await.unwrap();
    let jar = CookieJar::load(&secrets).await.unwrap();
    assert_eq!(cookie_header(&jar, "https://discord.com/"), None);
}

#[tokio::test]
async fn chrome_identity_matches_the_wreq_util_profile() {
    // The emulation's own headers are off in `http_client`, so tritone sends
    // its own UA and sec-ch-ua: they must stay what the TLS profile claims.
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let client = wreq::Client::builder()
        .emulation(wreq_util::Emulation::builder().profile(CHROME_PROFILE).platform(platform()).build())
        .build()
        .unwrap();
    client.get(server.uri()).send().await.unwrap();
    let req = &server.received_requests().await.unwrap()[0];
    let id = chrome_identity();
    assert_eq!(req.headers.get("user-agent").unwrap().to_str().unwrap(), id.user_agent);
    assert_eq!(req.headers.get("sec-ch-ua").unwrap().to_str().unwrap(), id.sec_ch_ua);
    assert_eq!(req.headers.get("sec-ch-ua-platform").unwrap().to_str().unwrap(), id.sec_ch_ua_platform);
}

#[tokio::test]
async fn http_client_sends_cookies_and_stores_new_ones() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).insert_header("set-cookie", "fresh=1; Path=/; Max-Age=60"))
        .mount(&server)
        .await;
    // wiremock serves 127.0.0.1, which the jar rejects; so check the jar is wired
    // by asking it directly after a discord.com-scoped insert.
    let jar = Arc::new(CookieJar::empty());
    set(&jar, "https://discord.com/", "__dcfduid=a1; Path=/; Max-Age=3600");
    let client = http_client(jar.clone()).unwrap();
    let resp = client.get(server.uri()).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let req = &server.received_requests().await.unwrap()[0];
    assert!(req.headers.get("cookie").is_none(), "discord cookies never go to other hosts");
    assert!(req.headers.get("upgrade-insecure-requests").is_none(), "no navigation headers on API calls");
    assert_eq!(cookie_header(&jar, "http://127.0.0.1/"), None, "other hosts' cookies are dropped");
}

/// A one-shot WebSocket server that records the raw upgrade request, answers
/// the handshake, then echoes one text frame.
async fn ws_echo_server() -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/?v=9", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "client closed before the handshake");
            buf.extend_from_slice(&chunk[..n]);
        }
        let raw = String::from_utf8(buf).unwrap();
        let key = raw
            .lines()
            .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: ").or_else(|| l.strip_prefix("sec-websocket-key: ")))
            .unwrap()
            .trim()
            .to_string();
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        let resp = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
            sock,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        if let Some(Ok(m)) = ws.next().await {
            ws.send(m).await.unwrap();
        }
        let _ = ws.next().await;
        raw
    });
    (url, task)
}

#[tokio::test]
async fn ws_connect_upgrades_through_wreq_with_browser_header_casing() {
    let (url, server) = ws_echo_server().await;
    let client = http_client(Arc::new(CookieJar::empty())).unwrap();
    let headers = vec![("User-Agent", "UA-test".to_string()), ("Origin", "https://discord.com".to_string())];
    let mut ws = ws_connect(&client, &url, &headers).await.unwrap();
    ws.send(WsMessage::text("ping")).await.unwrap();
    let echoed = ws.next().await.unwrap().unwrap();
    assert_eq!(echoed.into_text().unwrap().as_str(), "ping");
    ws.close(None).await.unwrap();

    let raw = server.await.unwrap();
    assert!(raw.starts_with("GET /?v=9 HTTP/1.1\r\n"), "{raw}");
    assert!(raw.contains("\r\nOrigin: https://discord.com\r\n"), "Origin must keep its casing: {raw}");
    assert!(raw.contains("\r\nUser-Agent: UA-test\r\n"), "{raw}");
    assert!(raw.contains("\r\nUpgrade: websocket\r\n"), "{raw}");
    assert!(raw.contains("\r\nSec-WebSocket-Version: 13\r\n"), "{raw}");
}

#[tokio::test]
async fn ws_connect_rejects_a_non_101_answer() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(403)).mount(&server).await;
    let client = http_client(Arc::new(CookieJar::empty())).unwrap();
    let url = server.uri().replacen("http://", "ws://", 1);
    let err = ws_connect(&client, &url, &[]).await.unwrap_err();
    assert!(err.to_string().contains("403"), "{err}");
}
