//! The connection loop: connect, identify or resume, heartbeat, dispatch to
//! `LiveState`, and decide what to do when the socket ends.
//!
//! Protocol: https://docs.discord.food/gateway/using-gateway (heartbeating,
//! resuming), close codes from
//! https://docs.discord.food/gateway/opcodes-and-close-codes#gateway-close-event-codes
//!
//! Never closes with 1000/1001: that ends the session and shows the user
//! offline. Every close tritone sends is 4000, which keeps the session resumable.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::GatewayEvent;
use super::events::parse_dispatch;
use super::ready::{parse_ready, supplemental_roles};
use super::subscribe::{full_watch_channels, missed_guilds, subscribe_channels, subscription_packet};
use super::zlib::Inflater;
use crate::discord::DiscordApi;
use crate::discord::fingerprint::Fingerprint;
use crate::discord::transport::{WsStream, ws_connect};
use crate::discord::types::{DiscordError, Id};
use crate::domain::live::ConnState;
use crate::mcp::state::AppState;
use crate::secrets::ACCOUNT_TOKEN;
use crate::store::WatchMap;

pub(crate) const GATEWAY_URL: &str = "wss://gateway.discord.gg";
/// Appended to the gateway and resume URLs alike.
const QUERY: &str = "/?v=9&encoding=json&compress=zlib-stream";
/// concord's user set: bits 0, 2–7. No proto settings, no token refresh.
const CAPABILITIES: u64 = 253;
/// Sent on every close tritone makes: anything but 1000/1001 keeps the session.
const CLOSE_RESUMABLE: u16 = 4000;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// Resume attempts that never reach RESUMED before identifying fresh.
const MAX_RESUME_ATTEMPTS: u32 = 3;

/// Every wait the loop makes. Tests shrink them; production uses `default()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timing {
    pub connect: Duration,
    pub hello: Duration,
    /// Backoff: `min(cap, base << n)` plus up to `jitter`.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    pub jitter: Duration,
    /// Before a fresh identify after op 9 false or close 4007/4009.
    pub identify_min: Duration,
    pub identify_max: Duration,
    /// A connection up this long resets the backoff.
    pub stable_after: Duration,
}

impl Default for Timing {
    /// Connect 15 s and HELLO 10 s (Serein's), backoff 1 s doubling to 30 s
    /// plus 0–1 s jitter, reset after a stable minute, 1–5 s before a fresh
    /// identify (https://docs.discord.food/gateway/gateway-events#invalid-session).
    fn default() -> Timing {
        Timing {
            connect: Duration::from_secs(15),
            hello: Duration::from_secs(10),
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
            jitter: Duration::from_secs(1),
            identify_min: Duration::from_secs(1),
            identify_max: Duration::from_secs(5),
            stable_after: Duration::from_secs(60),
        }
    }
}

/// A random duration in `lo..=hi`.
fn between(lo: Duration, hi: Duration) -> Duration {
    let (lo, hi) = (lo.as_millis() as u64, hi.as_millis().max(lo.as_millis()) as u64);
    Duration::from_millis(rand::random_range(lo..=hi))
}

/// Builds the REST client and the HTTP client (shared with the WebSocket
/// upgrade) for a token. Production reloads the cookie jar first.
#[async_trait]
pub(crate) trait Opener: Send + Sync {
    async fn open(&self, token: &str) -> anyhow::Result<(Arc<dyn DiscordApi>, wreq::Client)>;
}

/// What lets a later connection resume instead of identifying.
#[derive(Debug, Clone)]
struct Session {
    id: String,
    resume_url: String,
}

/// What came before a reconnect, to spot messages that never arrived live.
struct Gap {
    /// Highest message id seen per channel when the socket dropped.
    seen: HashMap<Id, Id>,
    /// The drop's time as a snowflake: newer messages are expected gaps.
    cutoff: Id,
}

/// How one connection ended, and what to do next.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Resume,
    /// Identify fresh after waiting at least this long.
    Identify(Duration),
    /// Halted (bad token, fatal close): wait for a new login.
    Halt,
    /// A new token is in the keyring: open a new session with it.
    NewToken(String),
}

struct Ended {
    next: Next,
    /// READY or RESUMED arrived on this connection.
    ready: bool,
    lasted: Duration,
}

/// `min(cap, base << n)` plus jitter (`next` is the deterministic part;
/// `delay` adds the jitter).
pub(crate) struct Backoff {
    n: u32,
    timing: Timing,
}

impl Backoff {
    pub(crate) fn new(timing: Timing) -> Backoff {
        Backoff { n: 0, timing }
    }

    pub(crate) fn next(&mut self) -> Duration {
        let d = self.timing.backoff_base.saturating_mul(1u32 << self.n.min(16)).min(self.timing.backoff_cap);
        self.n = self.n.saturating_add(1);
        d
    }

    fn delay(&mut self) -> Duration {
        self.next() + between(Duration::ZERO, self.timing.jitter)
    }

    pub(crate) fn reset(&mut self) {
        self.n = 0;
    }
}

/// Op 2: Chrome web's properties, presence idle so phone notifications keep
/// coming, cold-start `client_state`, no intents (user accounts don't use them).
/// https://docs.discord.food/gateway/gateway-events#identify
pub(crate) fn identify_payload(token: &str, fp: &Fingerprint) -> Value {
    json!({
        "op": 2,
        "d": {
            "token": token,
            "capabilities": CAPABILITIES,
            "properties": fp.identify_properties(),
            "presence": {"status": "unknown", "since": 0, "activities": [], "afk": true},
            "compress": false,
            "client_state": {
                "guild_versions": {},
                "highest_last_message_id": "0",
                "read_state_version": 0,
                "user_guild_settings_version": -1,
                "user_settings_version": -1,
                "private_channels_version": "0",
                "api_code_version": 0,
            },
        }
    })
}

fn resume_payload(token: &str, session: &Session, seq: Option<u64>) -> Value {
    json!({"op": 6, "d": {"token": token, "session_id": session.id, "seq": seq}})
}

/// The token only ever goes to a Discord gateway host (or, in tests, the host
/// tritone was pointed at).
pub(crate) fn resume_url_allowed(resume: &str, main: &str) -> bool {
    let (Ok(r), Ok(m)) = (url::Url::parse(resume), url::Url::parse(main)) else { return false };
    let Some(host) = r.host_str() else { return false };
    let discord = r.scheme() == "wss" && (host == "discord.gg" || host.ends_with(".discord.gg"));
    discord || (r.scheme() == m.scheme() && r.host_str() == m.host_str() && r.port() == m.port())
}

/// Close codes that end the session for good: every re-identify would add a
/// session, so stop and surface them.
fn is_fatal(code: u16) -> bool {
    matches!(code, 4001..=4003 | 4005 | 4008 | 4010..=4016)
}

pub(crate) struct Gateway {
    state: AppState,
    opener: Arc<dyn Opener>,
    fp: Arc<Fingerprint>,
    url: String,
    timing: Timing,
}

impl Gateway {
    /// `url` is the gateway base (`GATEWAY_URL` in production), without query.
    pub(crate) fn new(state: AppState, opener: Arc<dyn Opener>, fp: Arc<Fingerprint>, url: String) -> Gateway {
        Gateway { state, opener, fp, url, timing: Timing::default() }
    }

    #[cfg(test)]
    pub(crate) fn with_timing(mut self, timing: Timing) -> Gateway {
        self.timing = timing;
        self
    }

    fn identify_delay(&self) -> Duration {
        between(self.timing.identify_min, self.timing.identify_max)
    }

    fn apply(&self, ev: GatewayEvent) {
        self.state.live.apply(ev, self.state.timer.now_ms());
    }

    /// Runs for the daemon's lifetime.
    pub(crate) async fn run(self) {
        let mut backoff = Backoff::new(self.timing);
        let mut pending: Option<String> = None;
        loop {
            let token = match pending.take() {
                Some(t) => t,
                None => self.wait_for_token().await,
            };
            let client = match self.opener.open(&token).await {
                Ok((api, client)) => {
                    self.state.set_api(Some(api));
                    client
                }
                Err(e) => {
                    tracing::warn!("building the Discord client failed: {e:#}");
                    sleep(backoff.delay()).await;
                    pending = Some(token);
                    continue;
                }
            };
            match self.session_loop(&client, &token, &mut backoff).await {
                Next::NewToken(t) => pending = Some(t),
                _ => self.wait_for_login().await,
            }
        }
    }

    /// The keyring's token. Without one, halts with `NeedsLogin` and waits
    /// for `status` to find a new login.
    async fn wait_for_token(&self) -> String {
        loop {
            match self.state.secrets.get(ACCOUNT_TOKEN).await {
                Ok(Some(t)) if !t.is_empty() => return t,
                Ok(_) => tracing::info!("no Discord token in the keyring; waiting for `tritone login`"),
                Err(e) => tracing::warn!("reading the token from the keyring failed: {e:#}"),
            }
            self.state.live.halt(DiscordError::NeedsLogin);
            self.wait_for_login().await;
        }
    }

    /// Until `status` accepts a new token (`try_login_with` lifts the halt,
    /// then notifies). A stale wake-up while still halted waits again, so a
    /// dead token never drives an identify loop.
    async fn wait_for_login(&self) {
        loop {
            self.state.login_notify.notified().await;
            if !matches!(self.state.live.connection(), ConnState::Halted(_)) {
                return;
            }
        }
    }

    /// Connects and reconnects with one token until it halts or a new token
    /// replaces it.
    async fn session_loop(&self, client: &wreq::Client, token: &str, backoff: &mut Backoff) -> Next {
        let mut session: Option<Session> = None;
        let mut seq: Option<u64> = None;
        let mut gap: Option<Gap> = None;
        let mut resume_attempts = 0;
        loop {
            let resuming = session.is_some();
            let ended = self.connect_once(client, token, &mut session, &mut seq, &mut gap).await;
            if ended.ready || !resuming {
                resume_attempts = 0;
            } else {
                resume_attempts += 1;
            }
            if ended.ready && ended.lasted >= self.timing.stable_after {
                backoff.reset();
            }
            let wait = match ended.next {
                // The socket is gone: never leave the model looking ready, or
                // lifting the halt (the queue's resume) would let writes through.
                Next::Halt => {
                    self.apply(GatewayEvent::Disconnected);
                    return Next::Halt;
                }
                Next::NewToken(_) => return ended.next,
                Next::Resume if resume_attempts < MAX_RESUME_ATTEMPTS => backoff.delay(),
                Next::Resume => {
                    tracing::info!(attempts = resume_attempts, "resume keeps failing; identifying fresh");
                    session = None;
                    backoff.delay()
                }
                Next::Identify(min) => {
                    session = None;
                    min.max(backoff.delay())
                }
            };
            // From the first drop since READY/RESUMED; a fresh READY reads it.
            if gap.is_none() && !self.state.live.channels().is_empty() {
                gap = Some(self.gap());
            }
            self.apply(GatewayEvent::Disconnected);
            let until = Instant::now() + wait;
            loop {
                tokio::select! {
                    _ = sleep_until(until) => break,
                    _ = self.state.login_notify.notified() => {
                        if let Some(t) = self.changed_token(token).await {
                            return Next::NewToken(t);
                        }
                    }
                }
            }
        }
    }

    fn gap(&self) -> Gap {
        let seen = self.state.live.channels().into_iter().filter_map(|c| Some((c.id, c.last_message_id?))).collect();
        Gap { seen, cutoff: Id::from_timestamp_ms(self.state.timer.now_ms(), 0) }
    }

    /// After a login notify: the keyring's token when it differs from the
    /// one in use (C2: a REST 401 can come before, or without, a gateway
    /// 4004). Awaited outside `select!`, so a wake-up is never lost mid-read.
    async fn changed_token(&self, current: &str) -> Option<String> {
        match self.state.secrets.get(ACCOUNT_TOKEN).await {
            Ok(Some(t)) if !t.is_empty() && t != current => Some(t),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!("reading the token from the keyring failed: {e:#}");
                None
            }
        }
    }

    /// One socket, from connect to close.
    async fn connect_once(
        &self,
        client: &wreq::Client,
        token: &str,
        session: &mut Option<Session>,
        seq: &mut Option<u64>,
        gap: &mut Option<Gap>,
    ) -> Ended {
        let ended = |next| Ended { next, ready: false, lasted: Duration::ZERO };
        let base = session.as_ref().map_or(self.url.as_str(), |s| s.resume_url.as_str());
        let url = format!("{}{QUERY}", base.trim_end_matches('/'));
        let headers = self.fp.ws_headers();
        let ws = match timeout(self.timing.connect, ws_connect(client, &url, &headers)).await {
            Ok(Ok(ws)) => ws,
            Ok(Err(e)) => {
                tracing::warn!(resume = session.is_some(), "gateway connect failed: {e:#}");
                return ended(Next::Resume);
            }
            Err(_) => {
                tracing::warn!(resume = session.is_some(), "gateway connect timed out");
                return ended(Next::Resume);
            }
        };
        let mut conn = Conn { ws, z: Inflater::new() };
        let interval = match timeout(self.timing.hello, conn.recv()).await {
            Ok(Recv::Payload(v)) if v["op"] == 10 => v["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250),
            Ok(Recv::Closed(code)) => return ended(self.on_close(code, token)),
            _ => {
                tracing::warn!("gateway sent no HELLO");
                conn.close().await;
                return ended(Next::Resume);
            }
        };
        let first = match session {
            Some(s) => resume_payload(token, s, *seq),
            None => identify_payload(token, &self.fp),
        };
        tracing::info!(resume = session.is_some(), "gateway connected");
        if conn.send(&first).await.is_err() {
            return ended(Next::Resume);
        }
        let mut live = Live { gw: self, token, session, seq, gap, ready_at: None };
        let next = live.pump(&mut conn, Duration::from_millis(interval.max(1))).await;
        let ready_at = live.ready_at;
        Ended { next, ready: ready_at.is_some(), lasted: ready_at.map_or(Duration::ZERO, |t| t.elapsed()) }
    }

    /// What a close code means. Fatal ones halt here.
    fn on_close(&self, code: Option<u16>, token: &str) -> Next {
        tracing::info!(code, "gateway closed");
        match code {
            Some(4004) => {
                self.state.live.reject_token(token);
                Next::Halt
            }
            Some(c) if is_fatal(c) => {
                self.state.live.halt(DiscordError::GatewayClosed { code: c });
                Next::Halt
            }
            Some(4007 | 4009) => Next::Identify(self.identify_delay()),
            _ => Next::Resume,
        }
    }

    /// After a fresh READY that follows a reconnect: op 37 for the full-watch
    /// channels of guilds whose messages didn't arrive live.
    fn subscriptions(
        &self,
        snap: &crate::discord::types::ReadySnapshot,
        gap: &Gap,
        watched: &HashSet<Id>,
    ) -> Vec<Value> {
        let missed = missed_guilds(snap, &gap.seen, gap.cutoff);
        let packets: Vec<Value> = missed
            .into_iter()
            .filter_map(|g| {
                let channels = subscribe_channels(&snap.channels, g, watched);
                (!channels.is_empty()).then(|| subscription_packet(g, &channels))
            })
            .collect();
        if !packets.is_empty() {
            tracing::info!(guilds = packets.len(), "messages went missing; subscribing those guilds");
        }
        packets
    }
}

enum Recv {
    Payload(Value),
    Closed(Option<u16>),
}

struct Conn {
    ws: WsStream,
    z: Inflater,
}

impl Conn {
    /// The next gateway payload. Cancel-safe: partial frames stay in the inflater.
    async fn recv(&mut self) -> Recv {
        loop {
            let bytes = match self.ws.next().await {
                Some(Ok(WsMessage::Binary(b))) => match self.z.push(&b) {
                    Ok(Some(full)) => full,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!("gateway stream corrupt: {e:#}");
                        return Recv::Closed(None);
                    }
                },
                Some(Ok(WsMessage::Text(t))) => t.as_bytes().to_vec(),
                Some(Ok(WsMessage::Close(f))) => return Recv::Closed(f.map(|f| u16::from(f.code))),
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    tracing::info!("gateway socket error: {e}");
                    return Recv::Closed(None);
                }
                None => return Recv::Closed(None),
            };
            match serde_json::from_slice(&bytes) {
                Ok(v) => return Recv::Payload(v),
                Err(e) => tracing::warn!(bytes = bytes.len(), "unparseable gateway payload: {e}"),
            }
        }
    }

    async fn send(&mut self, v: &Value) -> Result<(), ()> {
        self.ws.send(WsMessage::Text(v.to_string().into())).await.map_err(|e| {
            tracing::info!("gateway send failed: {e}");
        })
    }

    /// Closes with 4000, never 1000/1001, so the session stays resumable.
    async fn close(&mut self) {
        let frame = CloseFrame { code: CloseCode::from(CLOSE_RESUMABLE), reason: "".into() };
        let _ = timeout(CLOSE_TIMEOUT, self.ws.close(Some(frame))).await;
    }
}

/// One connected socket's loop.
struct Live<'a> {
    gw: &'a Gateway,
    token: &'a str,
    session: &'a mut Option<Session>,
    seq: &'a mut Option<u64>,
    gap: &'a mut Option<Gap>,
    ready_at: Option<Instant>,
}

impl Live<'_> {
    async fn pump(&mut self, conn: &mut Conn, interval: Duration) -> Next {
        // The first beat at a random point of the interval, so many clients
        // reconnecting at once don't beat in step.
        let jitter = interval.mul_f64(rand::random_range(0.0..1.0));
        let mut beat_at = Instant::now() + jitter;
        let mut acked = true;
        loop {
            tokio::select! {
                _ = sleep_until(beat_at) => {
                    if !acked {
                        tracing::info!("no heartbeat ack: zombie connection, resuming");
                        conn.close().await;
                        return Next::Resume;
                    }
                    acked = false;
                    beat_at = Instant::now() + interval;
                    if conn.send(&json!({"op": 1, "d": *self.seq})).await.is_err() {
                        return Next::Resume;
                    }
                }
                got = conn.recv() => {
                    let v = match got {
                        Recv::Payload(v) => v,
                        Recv::Closed(code) => return self.gw.on_close(code, self.token),
                    };
                    match self.on_payload(conn, v, &mut acked).await {
                        Some(next) => return next,
                        None => continue,
                    }
                }
                _ = self.gw.state.login_notify.notified() => {
                    if let Some(t) = self.gw.changed_token(self.token).await {
                        tracing::info!("new login found; reconnecting with it");
                        conn.close().await;
                        return Next::NewToken(t);
                    }
                }
            }
        }
    }

    /// Handles one payload; `Some` ends the connection.
    async fn on_payload(&mut self, conn: &mut Conn, v: Value, acked: &mut bool) -> Option<Next> {
        if let Some(s) = v.get("s").and_then(Value::as_u64) {
            *self.seq = Some(s);
        }
        match v.get("op").and_then(Value::as_u64) {
            Some(0) => {
                let t = v.get("t").and_then(Value::as_str).unwrap_or_default();
                let d = v.get("d").unwrap_or(&Value::Null);
                for p in self.on_dispatch(t, d) {
                    if conn.send(&p).await.is_err() {
                        return Some(Next::Resume);
                    }
                }
                None
            }
            Some(1) => conn.send(&json!({"op": 1, "d": *self.seq})).await.err().map(|()| Next::Resume),
            Some(11) => {
                *acked = true;
                None
            }
            Some(7) => {
                tracing::info!("gateway asked to reconnect");
                conn.close().await;
                Some(Next::Resume)
            }
            Some(9) => {
                let resumable = v.get("d").and_then(Value::as_bool).unwrap_or(false);
                tracing::info!(resumable, "gateway invalidated the session");
                conn.close().await;
                Some(if resumable { Next::Resume } else { Next::Identify(self.gw.identify_delay()) })
            }
            _ => None,
        }
    }

    /// Applies a dispatch; returns payloads to send back (op 37).
    fn on_dispatch(&mut self, t: &str, d: &Value) -> Vec<Value> {
        match t {
            "READY" => self.on_ready(d),
            "RESUMED" => {
                tracing::info!("gateway resumed");
                self.ready_at = Some(Instant::now());
                // Resuming replays what was missed: no gap to look for.
                *self.gap = None;
                self.gw.apply(GatewayEvent::Resumed);
                vec![]
            }
            "READY_SUPPLEMENTAL" => {
                let me = self.gw.state.live.me().map(|u| u.id).unwrap_or_default();
                for ev in supplemental_roles(d, me) {
                    self.gw.apply(ev);
                }
                vec![]
            }
            _ => {
                let me = self.gw.state.live.me().map(|u| u.id);
                for ev in parse_dispatch(t, d, me) {
                    self.gw.apply(ev);
                }
                vec![]
            }
        }
    }

    fn on_ready(&mut self, d: &Value) -> Vec<Value> {
        let snap = parse_ready(d, None);
        let resume_url = if resume_url_allowed(&snap.resume_url, &self.gw.url) {
            snap.resume_url.clone()
        } else {
            tracing::warn!("READY's resume URL isn't a Discord gateway; resuming through the main one");
            self.gw.url.clone()
        };
        *self.session = Some(Session { id: snap.session_id.clone(), resume_url });
        self.ready_at = Some(Instant::now());

        let watch = self.gw.state.store.all_watch().unwrap_or_else(|e| {
            tracing::warn!("reading watch levels failed: {e:#}");
            WatchMap::default()
        });
        let buffered = full_watch_channels(&snap.channels, &watch);
        let packets = self.gap.take().map(|g| self.gw.subscriptions(&snap, &g, &buffered)).unwrap_or_default();
        tracing::info!(guilds = snap.guilds.len(), channels = snap.channels.len(), "gateway READY");
        self.gw.apply(GatewayEvent::Ready(Box::new(snap)));
        self.gw.state.live.set_buffered(buffered);
        packets
    }
}

#[cfg(test)]
#[path = "conn_tests.rs"]
mod tests;
