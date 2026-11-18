//! Login: remote-auth v2 (the QR code the phone app scans), and checking a
//! pasted token. https://docs.discord.food/remote-authentication/desktop
//!
//! Nothing here logs the ticket, the token, or the decrypted user payload
//! (it holds the user's id and name).

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use rsa::pkcs8::EncodePublicKey;
use rsa::rand_core::OsRng;
use rsa::{Oaep, RsaPrivateKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::fingerprint::Fingerprint;
use super::transport::{WsStream, ws_connect};

/// The remote-auth gateway, v2 (v1 is gone).
pub const REMOTE_AUTH_URL: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
/// A QR session expires (close 4003) after `timeout_ms`; a new one is opened this many times.
const REGENERATIONS: u32 = 3;
const REGENERATE_DELAY: Duration = Duration::from_secs(1);
const DEFAULT_HEARTBEAT_MS: u64 = 41_250;
/// Used when hello carries no `timeout_ms`; Discord's is about 2–5 minutes.
const DEFAULT_SESSION_MS: u64 = 300_000;
const RSA_BITS: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    #[error("Discord asked for a CAPTCHA, which tritone never solves")]
    Captcha,
    #[error("the login was cancelled on the phone")]
    Cancelled,
    #[error("the QR code expired without being scanned")]
    Expired,
    #[error("that doesn't look like a Discord token: {0}")]
    InvalidToken(&'static str),
    #[error("the QR login went wrong: {0}")]
    Protocol(String),
    #[error("network error during login: {0}")]
    Http(String),
}

/// Where the QR flow connects. Tests point both at local servers.
pub struct LoginEndpoints {
    pub remote_auth: String,
    /// The API root, e.g. `https://discord.com/api/v9`.
    pub api: String,
}

impl LoginEndpoints {
    pub fn discord() -> LoginEndpoints {
        LoginEndpoints { remote_auth: REMOTE_AUTH_URL.into(), api: super::http::API_BASE.into() }
    }
}

/// Makes the RSA key for one QR session (tests reuse one: generation is slow in debug builds).
pub type KeySource<'a> = &'a (dyn Fn() -> Result<RsaPrivateKey, LoginError> + Sync);

fn new_key() -> Result<RsaPrivateKey, LoginError> {
    RsaPrivateKey::new(&mut OsRng, RSA_BITS)
        .map_err(|e| LoginError::Protocol(format!("RSA key generation failed: {e}")))
}

/// The QR login. `show` gets `https://discord.com/ra/<fingerprint>` each time
/// a QR code is ready (again after each expiry). Returns the token.
pub async fn qr_login(
    client: &wreq::Client,
    fp: &Fingerprint,
    show: &(dyn Fn(&str) + Sync),
) -> Result<String, LoginError> {
    let key = || new_key();
    qr_login_with(client, fp, show, &LoginEndpoints::discord(), &key).await
}

pub async fn qr_login_with(
    client: &wreq::Client,
    fp: &Fingerprint,
    show: &(dyn Fn(&str) + Sync),
    ends: &LoginEndpoints,
    key: KeySource<'_>,
) -> Result<String, LoginError> {
    // Same client, same cookie jar: the fingerprint and the ticket exchange
    // must look like one browser.
    let x_fingerprint = fetch_x_fingerprint(client, fp, &ends.api).await;
    for attempt in 0..=REGENERATIONS {
        if attempt > 0 {
            tokio::time::sleep(REGENERATE_DELAY).await;
        }
        // A fresh key per QR: the code is the key's hash. Generation blocks
        // for a moment, before any socket is open.
        let key = key()?;
        match session(client, fp, &ends.remote_auth, &key, show).await? {
            Session::Ticket(ticket) => {
                return exchange_ticket(client, fp, &ends.api, x_fingerprint.as_deref(), &ticket, &key).await;
            }
            Session::Expired => tracing::info!("QR code expired; showing a new one"),
        }
    }
    Err(LoginError::Expired)
}

enum Session {
    Ticket(String),
    Expired,
}

/// The unauthenticated `GET /experiments` gives the `X-Fingerprint` login
/// requests carry (https://docs.discord.food/authentication#fingerprints).
/// Missing is tolerated: the exchange is tried without it.
async fn fetch_x_fingerprint(client: &wreq::Client, fp: &Fingerprint, api: &str) -> Option<String> {
    let mut req = client.get(format!("{api}/experiments"));
    for (name, value) in fp.rest_headers(false) {
        req = req.header(name, value);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::warn!(status = resp.status().as_u16(), "login fingerprint fetch failed");
        return None;
    }
    let v: Value = serde_json::from_slice(&resp.bytes().await.ok()?).ok()?;
    v.get("fingerprint").and_then(Value::as_str).map(str::to_string)
}

fn protocol(what: &str) -> LoginError {
    LoginError::Protocol(what.to_string())
}

/// Decrypts the server's nonce and returns the proof: base64url, no padding.
pub fn nonce_proof(key: &RsaPrivateKey, encrypted_b64: &str) -> Result<String, LoginError> {
    let encrypted = STANDARD.decode(encrypted_b64.trim()).map_err(|_| protocol("the nonce isn't base64"))?;
    let nonce = key.decrypt(Oaep::new::<Sha256>(), &encrypted).map_err(|_| protocol("the nonce didn't decrypt"))?;
    Ok(URL_SAFE_NO_PAD.encode(nonce))
}

async fn send_op(ws: &mut WsStream, v: Value) -> Result<(), LoginError> {
    ws.send(WsMessage::text(v.to_string())).await.map_err(|e| LoginError::Http(format!("remote-auth send failed: {e}")))
}

/// One QR session, from hello to a ticket, an expiry or a failure.
async fn session(
    client: &wreq::Client,
    fp: &Fingerprint,
    url: &str,
    key: &RsaPrivateKey,
    show: &(dyn Fn(&str) + Sync),
) -> Result<Session, LoginError> {
    let spki = key.to_public_key().to_public_key_der().map_err(|e| LoginError::Protocol(e.to_string()))?;
    let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(spki.as_bytes()));
    let mut ws = ws_connect(client, url, &fp.ws_headers()).await.map_err(|e| LoginError::Http(e.to_string()))?;

    let mut beat: Option<tokio::time::Interval> = None;
    let mut acked = true;
    let mut deadline = tokio::time::Instant::now() + Duration::from_millis(DEFAULT_SESSION_MS);
    loop {
        let frame = tokio::select! {
            _ = async { beat.as_mut().expect("guarded").tick().await }, if beat.is_some() => {
                if !acked {
                    // A missed ack means a dead socket: start over with a new QR.
                    tracing::info!("remote-auth heartbeat not acknowledged; reconnecting");
                    return Ok(Session::Expired);
                }
                acked = false;
                send_op(&mut ws, json!({"op": "heartbeat"})).await?;
                continue;
            }
            _ = tokio::time::sleep_until(deadline) => return Ok(Session::Expired),
            f = ws.next() => f,
        };
        let text = match frame {
            Some(Ok(WsMessage::Text(t))) => t,
            Some(Ok(WsMessage::Close(close))) => {
                let code = close.map_or(0, |c| u16::from(c.code));
                tracing::info!(code, "remote-auth closed");
                return if code == 4003 {
                    Ok(Session::Expired)
                } else {
                    Err(LoginError::Protocol(format!("closed with code {code}")))
                };
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(LoginError::Http(format!("remote-auth socket error: {e}"))),
            None => return Err(protocol("the connection ended")),
        };
        let v: Value = serde_json::from_str(&text).map_err(|_| protocol("a frame wasn't JSON"))?;
        let op = v.get("op").and_then(Value::as_str).unwrap_or("");
        tracing::debug!(op, "remote-auth op");
        match op {
            "hello" => {
                let every =
                    v.get("heartbeat_interval").and_then(Value::as_u64).unwrap_or(DEFAULT_HEARTBEAT_MS).max(1_000);
                let lifetime = v.get("timeout_ms").and_then(Value::as_u64).unwrap_or(DEFAULT_SESSION_MS);
                // A little past Discord's own expiry, which closes with 4003 first.
                deadline = tokio::time::Instant::now() + Duration::from_millis(lifetime + 10_000);
                let every = Duration::from_millis(every);
                let mut iv = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                beat = Some(iv);
                send_op(&mut ws, json!({"op": "init", "encoded_public_key": STANDARD.encode(spki.as_bytes())})).await?;
            }
            "nonce_proof" => {
                let enc = v
                    .get("encrypted_nonce")
                    .and_then(Value::as_str)
                    .ok_or_else(|| protocol("nonce_proof without a nonce"))?;
                let proof = nonce_proof(key, enc)?;
                send_op(&mut ws, json!({"op": "nonce_proof", "nonce": proof})).await?;
            }
            "pending_remote_init" => {
                let got = v.get("fingerprint").and_then(Value::as_str).unwrap_or("");
                if got != expected {
                    let _ = ws.close(None).await;
                    return Err(protocol("the server's QR fingerprint doesn't match our key"));
                }
                show(&format!("https://discord.com/ra/{got}"));
            }
            // The payload is the scanning user's id and name: never decrypted for logs.
            "pending_ticket" => tracing::info!("QR code scanned; confirm the login on the phone"),
            "pending_login" => {
                let ticket = v
                    .get("ticket")
                    .and_then(Value::as_str)
                    .ok_or_else(|| protocol("pending_login without a ticket"))?;
                let _ = ws.close(None).await;
                return Ok(Session::Ticket(ticket.to_string()));
            }
            "cancel" => {
                let _ = ws.close(None).await;
                return Err(LoginError::Cancelled);
            }
            "heartbeat_ack" => acked = true,
            other => tracing::debug!(op = other, "remote-auth op ignored"),
        }
    }
}

/// `POST /users/@me/remote-auth/login {ticket}` (no Authorization) returns
/// the token encrypted to our key.
async fn exchange_ticket(
    client: &wreq::Client,
    fp: &Fingerprint,
    api: &str,
    x_fingerprint: Option<&str>,
    ticket: &str,
    key: &RsaPrivateKey,
) -> Result<String, LoginError> {
    let mut req = client.post(format!("{api}/users/@me/remote-auth/login"));
    let mut headers = fp.rest_headers(true).into_iter();
    // Same place as the REST client's own headers: after Sec-Ch-Ua-Platform.
    for (name, value) in headers.by_ref().take(1) {
        req = req.header(name, value);
    }
    req = req.header("Content-Type", "application/json");
    if let Some(x) = x_fingerprint {
        req = req.header("X-Fingerprint", x);
    }
    for (name, value) in headers {
        req = req.header(name, value);
    }
    let resp = req
        .body(json!({"ticket": ticket}).to_string())
        .send()
        .await
        .map_err(|_| LoginError::Http("the ticket exchange failed".into()))?;
    let status = resp.status().as_u16();
    let body: Value = resp.bytes().await.ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    if body.get("captcha_key").is_some() {
        return Err(LoginError::Captcha);
    }
    if !(200..300).contains(&status) {
        return Err(LoginError::Http(format!("the ticket exchange answered HTTP {status}")));
    }
    let enc = body
        .get("encrypted_token")
        .and_then(Value::as_str)
        .ok_or_else(|| protocol("no encrypted_token in the answer"))?;
    let enc = STANDARD.decode(enc).map_err(|_| protocol("the token isn't base64"))?;
    let token = key.decrypt(Oaep::new::<Sha256>(), &enc).map_err(|_| protocol("the token didn't decrypt"))?;
    String::from_utf8(token).map_err(|_| protocol("the token isn't text"))
}

/// A pasted token: trims whitespace and one pair of quotes, then checks it
/// looks like a user token (rules as Serein's `crates/client-core/src/auth.rs`).
pub fn validate_pasted(raw: &str) -> Result<String, LoginError> {
    let t = raw.trim();
    let t = ['"', '\''].iter().find_map(|q| t.strip_prefix(*q).and_then(|s| s.strip_suffix(*q))).unwrap_or(t);
    if t.len() < 16 {
        return Err(LoginError::InvalidToken("too short"));
    }
    if t.len() > 2048 {
        return Err(LoginError::InvalidToken("too long"));
    }
    if t.starts_with("Bot ") || t.starts_with("Bearer ") {
        return Err(LoginError::InvalidToken("a user token has no `Bot ` or `Bearer ` prefix"));
    }
    // A quote left after the one pair came off is a paste mistake, not part of a token.
    if !t.bytes().all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\'') {
        return Err(LoginError::InvalidToken("it has spaces or characters a token never has"));
    }
    Ok(t.to_string())
}

#[cfg(test)]
#[path = "login_tests.rs"]
mod tests;
