use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use rsa::pkcs8::DecodePublicKey;
use rsa::rand_core::OsRng;
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::{LoginEndpoints, LoginError, nonce_proof, qr_login_with, validate_pasted};
use crate::discord::fingerprint::Fingerprint;
use crate::discord::transport::{CookieJar, http_client};

/// Key generation is slow in debug builds: one key for every test.
fn test_key() -> RsaPrivateKey {
    static KEY: OnceLock<RsaPrivateKey> = OnceLock::new();
    KEY.get_or_init(|| RsaPrivateKey::new(&mut OsRng, 2048).unwrap()).clone()
}

const TOKEN: &str = "fake-token-0001.abcdefgh";

#[test]
fn validate_pasted_rules() {
    let ok = "abcdefghijklmnop.qrstu";
    assert_eq!(validate_pasted(ok).unwrap(), ok);
    assert_eq!(validate_pasted(&format!("  \"{ok}\"\n")).unwrap(), ok);
    assert_eq!(validate_pasted(&format!("'{ok}'")).unwrap(), ok);
    // Only one pair of quotes comes off.
    assert!(validate_pasted(&format!("\"\"{ok}\"\"")).is_err());
    assert!(validate_pasted("short").is_err());
    assert!(validate_pasted(&"a".repeat(2049)).is_err());
    assert!(validate_pasted(&"a".repeat(2048)).is_ok());
    assert!(validate_pasted(&format!("Bot {ok}")).is_err());
    assert!(validate_pasted(&format!("Bearer {ok}")).is_err());
    assert!(validate_pasted("abcdefgh ijklmnopq").is_err(), "inner space");
    assert!(validate_pasted("abcdefghijklmnopñ").is_err(), "non-ASCII");
    assert!(validate_pasted("").is_err());
    assert!(matches!(validate_pasted("short"), Err(LoginError::InvalidToken(_))));
}

#[test]
fn nonce_proof_roundtrip() {
    let key = test_key();
    let nonce = b"a-random-nonce-from-discord-0123";
    let encrypted = key.to_public_key().encrypt(&mut OsRng, Oaep::new::<Sha256>(), nonce).unwrap();
    let proof = nonce_proof(&key, &STANDARD.encode(encrypted)).unwrap();
    assert_eq!(proof, URL_SAFE_NO_PAD.encode(nonce));
    assert!(!proof.contains('=') && !proof.contains('+') && !proof.contains('/'));
    assert!(matches!(nonce_proof(&key, "not base64!"), Err(LoginError::Protocol(_))));
}

/// What the fake remote-auth server does after `init`.
#[derive(Clone, Copy, PartialEq)]
enum Script {
    Login,
    WrongFingerprint,
    Cancel,
    Expire,
}

struct FakeRemoteAuth {
    url: String,
    /// The public key each session sent in `init`.
    keys: Arc<Mutex<Vec<RsaPublicKey>>>,
    sessions: Arc<Mutex<u32>>,
    origins: Arc<Mutex<Vec<String>>>,
}

fn text(v: Value) -> WsMessage {
    WsMessage::text(v.to_string())
}

async fn next_json(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Value {
    loop {
        match ws.next().await.expect("client open").expect("frame") {
            WsMessage::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["op"] != "heartbeat" {
                    return v;
                }
            }
            _ => continue,
        }
    }
}

// The handshake callback's `Result<Response, ErrorResponse>` is tungstenite's `Callback` signature.
#[allow(clippy::result_large_err)]
async fn fake_remote_auth(script: Script) -> FakeRemoteAuth {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/?v=2", listener.local_addr().unwrap());
    let keys = Arc::new(Mutex::new(Vec::new()));
    let sessions = Arc::new(Mutex::new(0));
    let origins = Arc::new(Mutex::new(Vec::new()));
    let (k, s, o) = (keys.clone(), sessions.clone(), origins.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            *s.lock().unwrap() += 1;
            let o = o.clone();
            let callback =
                move |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    let origin = req.headers().get("origin").and_then(|v| v.to_str().ok()).unwrap_or("");
                    o.lock().unwrap().push(origin.to_string());
                    Ok(resp)
                };
            let mut ws = tokio_tungstenite::accept_hdr_async(tcp, callback).await.unwrap();
            ws.send(text(json!({"op": "hello", "heartbeat_interval": 41250, "timeout_ms": 120000}))).await.unwrap();
            if script == Script::Expire {
                let frame = CloseFrame { code: CloseCode::Library(4003), reason: "expired".into() };
                let _ = ws.close(Some(frame)).await;
                continue;
            }
            let init = next_json(&mut ws).await;
            assert_eq!(init["op"], "init");
            let der = STANDARD.decode(init["encoded_public_key"].as_str().unwrap()).unwrap();
            let public = RsaPublicKey::from_public_key_der(&der).unwrap();
            k.lock().unwrap().push(public.clone());

            let nonce = b"server-nonce-0001";
            let enc = public.encrypt(&mut OsRng, Oaep::new::<Sha256>(), nonce).unwrap();
            ws.send(text(json!({"op": "nonce_proof", "encrypted_nonce": STANDARD.encode(enc)}))).await.unwrap();
            let proof = next_json(&mut ws).await;
            assert_eq!(proof, json!({"op": "nonce_proof", "nonce": URL_SAFE_NO_PAD.encode(nonce)}));

            let fingerprint = if script == Script::WrongFingerprint {
                URL_SAFE_NO_PAD.encode(Sha256::digest(b"some other key"))
            } else {
                URL_SAFE_NO_PAD.encode(Sha256::digest(&der))
            };
            ws.send(text(json!({"op": "pending_remote_init", "fingerprint": fingerprint}))).await.unwrap();
            let payload = public.encrypt(&mut OsRng, Oaep::new::<Sha256>(), b"1:0:0:U001").unwrap();
            let _ = ws
                .send(text(json!({"op": "pending_ticket", "encrypted_user_payload": STANDARD.encode(payload)})))
                .await;
            match script {
                Script::Cancel => {
                    let _ = ws.send(text(json!({"op": "cancel"}))).await;
                }
                _ => {
                    let _ = ws.send(text(json!({"op": "pending_login", "ticket": "ticket-0001"}))).await;
                }
            }
            let _ = ws.close(None).await;
        }
    });
    FakeRemoteAuth { url, keys, sessions, origins }
}

/// Answers the ticket exchange with the token encrypted to the session's key.
struct EncryptToken(Arc<Mutex<Vec<RsaPublicKey>>>);

impl Respond for EncryptToken {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let key = self.0.lock().unwrap().last().cloned().expect("a session key");
        let enc = key.encrypt(&mut OsRng, Oaep::new::<Sha256>(), TOKEN.as_bytes()).unwrap();
        ResponseTemplate::new(200).set_body_json(json!({"encrypted_token": STANDARD.encode(enc)}))
    }
}

async fn api_with_experiments() -> MockServer {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v9/experiments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"fingerprint": "1.fake-fp", "assignments": []})))
        .mount(&api)
        .await;
    api
}

async fn run(fake: &FakeRemoteAuth, api: &MockServer) -> (Result<String, LoginError>, Vec<String>) {
    let client = http_client(Arc::new(CookieJar::empty())).unwrap();
    let ends = LoginEndpoints { remote_auth: fake.url.clone(), api: format!("{}/api/v9", api.uri()) };
    let shown = Mutex::new(Vec::new());
    let show = |url: &str| shown.lock().unwrap().push(url.to_string());
    let key = || Ok(test_key());
    let out = qr_login_with(&client, &Fingerprint::for_this_machine(), &show, &ends, &key).await;
    (out, shown.into_inner().unwrap())
}

#[tokio::test]
async fn qr_flow_against_fake_server() {
    let fake = fake_remote_auth(Script::Login).await;
    let api = api_with_experiments().await;
    Mock::given(method("POST"))
        .and(path("/api/v9/users/@me/remote-auth/login"))
        .and(header("x-fingerprint", "1.fake-fp"))
        .and(body_json(json!({"ticket": "ticket-0001"})))
        .respond_with(EncryptToken(fake.keys.clone()))
        .expect(1)
        .mount(&api)
        .await;
    let (out, shown) = run(&fake, &api).await;
    assert_eq!(out.unwrap(), TOKEN);
    let der = rsa::pkcs8::EncodePublicKey::to_public_key_der(&test_key().to_public_key()).unwrap();
    assert_eq!(
        shown,
        vec![format!("https://discord.com/ra/{}", URL_SAFE_NO_PAD.encode(Sha256::digest(der.as_bytes())))]
    );
    assert_eq!(fake.origins.lock().unwrap().as_slice(), ["https://discord.com"]);
    // The exchange carries no Authorization.
    let reqs = api.received_requests().await.unwrap();
    assert!(reqs.iter().all(|r| r.headers.get("authorization").is_none()));
}

#[tokio::test]
async fn fingerprint_mismatch_fails() {
    let fake = fake_remote_auth(Script::WrongFingerprint).await;
    let api = api_with_experiments().await;
    let (out, shown) = run(&fake, &api).await;
    assert!(matches!(out, Err(LoginError::Protocol(_))), "{out:?}");
    assert!(shown.is_empty(), "a QR for someone else's key is never shown");
    assert!(api.received_requests().await.unwrap().iter().all(|r| r.url.path().ends_with("/experiments")));
}

#[tokio::test]
async fn ticket_exchange_captcha_is_captcha() {
    let fake = fake_remote_auth(Script::Login).await;
    let api = api_with_experiments().await;
    Mock::given(method("POST"))
        .and(path("/api/v9/users/@me/remote-auth/login"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "captcha_key": ["captcha-required"], "captcha_sitekey": "x", "captcha_service": "hcaptcha"
        })))
        .expect(1)
        .mount(&api)
        .await;
    let (out, _) = run(&fake, &api).await;
    assert_eq!(out, Err(LoginError::Captcha));
}

#[tokio::test]
async fn cancel_on_the_phone_is_cancelled() {
    let fake = fake_remote_auth(Script::Cancel).await;
    let api = api_with_experiments().await;
    let (out, _) = run(&fake, &api).await;
    assert_eq!(out, Err(LoginError::Cancelled));
}

#[tokio::test]
async fn close_4003_regenerates_up_to_three_times() {
    let fake = fake_remote_auth(Script::Expire).await;
    let api = api_with_experiments().await;
    let (out, _) = run(&fake, &api).await;
    assert_eq!(out, Err(LoginError::Expired));
    assert_eq!(*fake.sessions.lock().unwrap(), 4, "the first QR plus three regenerations");
}

#[test]
fn needs_login_tells_the_user_to_run_tritone_login() {
    let text = crate::discord::types::DiscordError::NeedsLogin.to_string();
    assert!(text.contains("run `tritone login`"), "{text}");
    assert!(!text.contains("try again"), "{text}");
}
