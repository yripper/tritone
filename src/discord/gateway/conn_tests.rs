//! The connection loop against a scripted local gateway: a tokio-tungstenite
//! server that speaks zlib-stream, driven on paused tokio time.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::Request;
use flate2::{Compress, Compression, FlushCompress};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Instant, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{Request as HsRequest, Response as HsResponse};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tower::ServiceExt;

use super::*;
use crate::config::Config;
use crate::discord::fake::FakeDiscord;
use crate::discord::fingerprint::Fingerprint;
use crate::discord::transport::{CookieJar, http_client};
use crate::discord::types::{DiscordError, Id};
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, ConnState, LiveState};
use crate::domain::send_queue::SendQueue;
use crate::mcp::http::app;
use crate::mcp::server::TritoneServer;
use crate::mcp::state::AppState;
use crate::secrets::fake::MemSecrets;
use crate::secrets::{ACCOUNT_TOKEN, SecretStore};
use crate::store::{Store, TargetKind, WatchLevel};
use crate::test_support::id_at;

/// The fake clock's "now": snowflakes in tests are built around it.
const T: i64 = 1_800_000_000_000;
const QUERY: &str = "/?v=9&encoding=json&compress=zlib-stream";

// ---- the scripted gateway ----

struct Server {
    listener: TcpListener,
    url: String,
}

impl Server {
    async fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        Server { listener, url }
    }

    /// The next client connection, after the WebSocket handshake.
    async fn accept(&self) -> Peer {
        let accepted = timeout(Duration::from_secs(10), self.listener.accept()).await;
        let (tcp, _) = accepted.expect("the gateway never connected").unwrap();
        let path = Arc::new(Mutex::new(String::new()));
        let seen = path.clone();
        // The callback's error type is tungstenite's (large) HTTP response.
        #[allow(clippy::result_large_err)]
        let record = move |req: &HsRequest, resp: HsResponse| {
            *seen.lock().unwrap() = req.uri().to_string();
            Ok(resp)
        };
        let ws = tokio_tungstenite::accept_hdr_async(tcp, record).await.unwrap();
        let path = path.lock().unwrap().clone();
        Peer { ws, z: Compress::new(Compression::default(), true), path, seq: 0 }
    }

    /// No new connection within `ms`: far longer than any test backoff.
    async fn no_connection_within(&self, ms: u64) -> bool {
        timeout(Duration::from_millis(ms), self.listener.accept()).await.is_err()
    }
}

#[derive(Debug)]
enum Got {
    Payload(Value),
    Closed(Option<u16>),
}

struct Peer {
    ws: WebSocketStream<TcpStream>,
    z: Compress,
    path: String,
    seq: u64,
}

impl Peer {
    /// One zlib-stream message, sync-flushed, as Discord frames it.
    async fn send(&mut self, v: Value) {
        let raw = v.to_string();
        let mut out = Vec::with_capacity(raw.len() + 64);
        self.z.compress_vec(raw.as_bytes(), &mut out, FlushCompress::Sync).unwrap();
        while out.len() == out.capacity() {
            out.reserve(1024);
            self.z.compress_vec(&[], &mut out, FlushCompress::Sync).unwrap();
        }
        self.ws.send(WsMessage::Binary(out.into())).await.unwrap();
    }

    async fn hello(&mut self, interval_ms: u64) {
        self.send(json!({"op": 10, "d": {"heartbeat_interval": interval_ms}})).await;
    }

    async fn dispatch(&mut self, t: &str, d: Value) {
        self.seq += 1;
        let seq = self.seq;
        self.send(json!({"op": 0, "t": t, "s": seq, "d": d})).await;
    }

    async fn got(&mut self) -> Got {
        loop {
            let next = timeout(Duration::from_secs(10), self.ws.next()).await.expect("nothing from the client");
            match next {
                Some(Ok(WsMessage::Text(t))) => return Got::Payload(serde_json::from_str(&t).unwrap()),
                Some(Ok(WsMessage::Close(f))) => return Got::Closed(f.map(|f| u16::from(f.code))),
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return Got::Closed(None),
            }
        }
    }

    /// The next payload that isn't a heartbeat. Panics on close.
    async fn recv_op(&mut self) -> Value {
        loop {
            match self.got().await {
                Got::Payload(v) if v["op"] == 1 => continue,
                Got::Payload(v) => return v,
                Got::Closed(code) => panic!("closed ({code:?}) while waiting for a payload"),
            }
        }
    }

    /// Reads until the client closes; returns its close code. Heartbeats are not acked.
    async fn until_closed(&mut self) -> Option<u16> {
        loop {
            if let Got::Closed(code) = self.got().await {
                return code;
            }
        }
    }

    async fn close(&mut self, code: u16) {
        let frame = CloseFrame { code: CloseCode::from(code), reason: "".into() };
        let _ = self.ws.close(Some(frame)).await;
    }

    /// HELLO, the identify, then READY. Returns the identify payload.
    async fn handshake(&mut self, ready: Value) -> Value {
        self.hello(41_250).await;
        let identify = self.recv_op().await;
        assert_eq!(identify["op"], 2, "expected identify, got {identify}");
        self.dispatch("READY", ready).await;
        identify
    }
}

fn ready_json(server: &Server, guilds: Value) -> Value {
    json!({
        "user": {"id": "1", "username": "U001"},
        "session_id": "sess-1",
        "resume_gateway_url": format!("{}/resume", server.url),
        "guilds": guilds,
        "private_channels": [],
        "read_state": {"entries": [], "partial": false, "version": 0},
        "user_guild_settings": {"entries": [], "partial": false, "version": 0},
    })
}

fn guild_json(gid: u64, channel: u64, last: Id) -> Value {
    json!({
        "id": gid.to_string(), "name": format!("G{gid:03}"), "owner_id": "999",
        "roles": [{"id": gid.to_string(), "permissions": "1024"}],
        "channels": [{"id": channel.to_string(), "type": 0, "name": "C", "last_message_id": last.to_string()}]
    })
}

fn message_json(id: Id, channel: u64, guild: u64) -> Value {
    json!({
        "id": id.to_string(), "channel_id": channel.to_string(), "guild_id": guild.to_string(), "type": 0,
        "content": "hello", "timestamp": "2027-01-15T08:00:00+00:00",
        "author": {"id": "2", "username": "U002"},
        "mentions": [], "mention_roles": [], "mention_everyone": false, "attachments": [], "embeds": []
    })
}

// ---- the gateway under test ----

/// Records each token it opens a session for; hands out a FakeDiscord.
#[derive(Default)]
struct FakeOpener(Mutex<Vec<String>>);

#[async_trait]
impl Opener for FakeOpener {
    async fn open(&self, token: &str) -> anyhow::Result<(Arc<dyn DiscordApi>, wreq::Client)> {
        self.0.lock().unwrap().push(token.to_string());
        let api: Arc<dyn DiscordApi> = Arc::new(FakeDiscord::default());
        Ok((api, http_client(Arc::new(CookieJar::empty()))?))
    }
}

struct Harness {
    state: AppState,
    opener: Arc<FakeOpener>,
    server: Server,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn harness(token: Option<&str>) -> Harness {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let timer = Arc::new(FakeTimer::at(T));
    let queue = Arc::new(SendQueue::new(store.clone(), timer.clone(), Arc::new(SeqGaps::new(&[0]))));
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 86_400_000 * 365 * 10 }));
    let secrets = Arc::new(MemSecrets::default());
    if let Some(t) = token {
        secrets.set(ACCOUNT_TOKEN, t).await.unwrap();
    }
    let state = AppState::new(store, secrets, live, queue, Arc::new(Config::default()), timer);
    let server = Server::start().await;
    let opener = Arc::new(FakeOpener::default());
    let fp = Arc::new(Fingerprint::for_this_machine());
    let gw = Gateway::new(state.clone(), opener.clone(), fp, server.url.clone()).with_timing(FAST);
    let task = tokio::spawn(gw.run());
    Harness { state, opener, server, task }
}

/// Real time, scaled down: backoff 20 ms doubling to 200 ms, 50–100 ms
/// before a fresh identify. Paused time can't be used for the loop itself:
/// tokio auto-advances it while loopback IO is in flight and fires the
/// connect and HELLO timeouts at random.
const FAST: Timing = Timing {
    connect: Duration::from_secs(5),
    hello: Duration::from_secs(5),
    backoff_base: Duration::from_millis(20),
    backoff_cap: Duration::from_millis(200),
    jitter: Duration::from_millis(10),
    identify_min: Duration::from_millis(50),
    identify_max: Duration::from_millis(100),
    stable_after: Duration::from_secs(60),
};

/// Lets the gateway task catch up with what the server sent.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Calls the `status` tool over HTTP and returns its JSON.
async fn status(state: &AppState) -> Value {
    let router = app(TritoneServer::new(state.clone(), None), 7781, "secret".into());
    let body =
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "status", "arguments": {}}});
    let req = Request::post("/mcp")
        .header("host", "127.0.0.1:7781")
        .header("authorization", "Bearer secret")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let rpc: Value = serde_json::from_slice(&bytes).unwrap();
    serde_json::from_str(rpc["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

// ---- tests ----

#[test]
fn identify_payload_shape() {
    let mut fp = Fingerprint::for_this_machine();
    fp.installation_id = Some("inst".into());
    let p = identify_payload("tok", &fp);
    assert_eq!(p["op"], 2);
    let d = &p["d"];
    assert_eq!(d["token"], "tok");
    assert_eq!(d["capabilities"], 253);
    assert_eq!(d["presence"], json!({"status": "unknown", "since": 0, "activities": [], "afk": true}));
    assert_eq!(d["compress"], false);
    assert!(d.get("intents").is_none());
    let props = d["properties"].as_object().unwrap();
    assert_eq!(props["browser"], "Chrome");
    assert_eq!(props["installation_id"], "inst");
    assert!(!props.contains_key("launch_signature") && !props.contains_key("has_client_mods"));
    assert_eq!(
        d["client_state"],
        json!({"guild_versions": {}, "highest_last_message_id": "0", "read_state_version": 0,
               "user_guild_settings_version": -1, "user_settings_version": -1,
               "private_channels_version": "0", "api_code_version": 0})
    );
}

#[test]
fn resume_url_must_be_a_discord_gateway_or_the_same_host() {
    assert!(resume_url_allowed("wss://gateway-us-east1-b.discord.gg", "wss://gateway.discord.gg"));
    assert!(resume_url_allowed("ws://127.0.0.1:9/resume", "ws://127.0.0.1:9"));
    assert!(!resume_url_allowed("wss://evil.example", "wss://gateway.discord.gg"));
    assert!(!resume_url_allowed("wss://discord.gg.evil.example", "wss://gateway.discord.gg"));
    assert!(!resume_url_allowed("", "wss://gateway.discord.gg"));
}

#[test]
fn backoff_doubles_to_a_cap_and_resets() {
    let mut b = Backoff::new(Timing::default());
    let secs: Vec<u64> = (0..7).map(|_| b.next().as_secs()).collect();
    assert_eq!(secs, vec![1, 2, 4, 8, 16, 30, 30]);
    b.reset();
    assert_eq!(b.next().as_secs(), 1);
}

#[tokio::test]
async fn identifies_and_applies_ready() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    assert_eq!(p.path, QUERY);
    let ready = ready_json(&h.server, json!([guild_json(10, 100, id_at(T - 60_000))]));
    let identify = p.handshake(ready).await;
    assert_eq!(identify["d"]["token"], "tok-1");
    settle().await;
    assert!(matches!(h.state.live.connection(), ConnState::Ready { .. }));
    assert_eq!(h.state.live.guilds().len(), 1);
    assert!(h.state.ctx().is_ok(), "the REST client is set");
    assert_eq!(*h.opener.0.lock().unwrap(), vec!["tok-1".to_string()]);
}

#[tokio::test]
async fn heartbeat_missed_ack_triggers_resume() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.hello(100).await;
    assert_eq!(p.recv_op().await["op"], 2);
    p.dispatch("READY", ready_json(&h.server, json!([]))).await;
    // The first beat (after a random 0..100 ms) goes unanswered; at the next
    // one, 100 ms later, the client gives up on the zombie with a non-1000 close.
    let start = Instant::now();
    let first = loop {
        if let Got::Payload(v) = p.got().await
            && v["op"] == 1
        {
            break v;
        }
    };
    assert_eq!(first["d"], 1, "the heartbeat carries the last seq");
    assert_eq!(p.until_closed().await, Some(4000));
    let waited = start.elapsed();
    assert!(waited >= Duration::from_millis(90) && waited <= Duration::from_millis(400), "{waited:?}");

    let mut p2 = h.server.accept().await;
    assert_eq!(p2.path, format!("/resume{QUERY}"));
    p2.hello(41_250).await;
    let resume = p2.recv_op().await;
    assert_eq!(resume, json!({"op": 6, "d": {"token": "tok-1", "session_id": "sess-1", "seq": 1}}));
    p2.dispatch("RESUMED", Value::Null).await;
    settle().await;
    assert!(matches!(h.state.live.connection(), ConnState::Ready { .. }));
}

#[tokio::test]
async fn server_heartbeat_request_is_answered_at_once() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.handshake(ready_json(&h.server, json!([]))).await;
    let start = Instant::now();
    p.send(json!({"op": 1, "d": null})).await;
    let beat = loop {
        if let Got::Payload(v) = p.got().await {
            break v;
        }
    };
    assert_eq!(beat, json!({"op": 1, "d": 1}));
    assert!(start.elapsed() < Duration::from_millis(100));
}

#[tokio::test]
async fn close_4004_halts_without_reidentify() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.hello(41_250).await;
    assert_eq!(p.recv_op().await["op"], 2);
    p.close(4004).await;
    settle().await;
    assert_eq!(h.state.live.connection(), ConnState::Halted(DiscordError::NeedsLogin));
    // Polling status with the same (rejected) token never wakes the gateway.
    for _ in 0..3 {
        let s = status(&h.state).await;
        assert_eq!(s["connection"], "needs_login");
        settle().await;
    }
    assert!(h.server.no_connection_within(1_000).await, "no second identify");
    assert!(h.state.ctx().is_err());
    assert_eq!(h.opener.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn fatal_close_codes_halt_without_retry() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.hello(41_250).await;
    p.recv_op().await;
    p.close(4011).await;
    settle().await;
    assert!(matches!(h.state.live.connection(), ConnState::Halted(DiscordError::Api { code: Some(4011), .. })));
    assert!(h.server.no_connection_within(1_000).await);
}

#[tokio::test]
async fn close_4009_identifies_fresh() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.handshake(ready_json(&h.server, json!([]))).await;
    p.close(4009).await;
    let mut p2 = h.server.accept().await;
    assert_eq!(p2.path, QUERY, "a fresh identify goes to the main gateway");
    p2.hello(41_250).await;
    assert_eq!(p2.recv_op().await["op"], 2);
}

#[tokio::test]
async fn op9_false_reidentifies_after_delay() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.handshake(ready_json(&h.server, json!([]))).await;
    let start = Instant::now();
    p.send(json!({"op": 9, "d": false})).await;
    let mut p2 = h.server.accept().await;
    let waited = start.elapsed();
    assert!(waited >= FAST.identify_min, "waited {waited:?}");
    assert!(waited <= Duration::from_secs(1), "waited {waited:?}");
    assert_eq!(p2.path, QUERY);
    p2.hello(41_250).await;
    assert_eq!(p2.recv_op().await["op"], 2, "identify, not resume");
}

#[tokio::test]
async fn op9_true_and_op7_resume() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.handshake(ready_json(&h.server, json!([]))).await;
    p.send(json!({"op": 7, "d": null})).await;
    assert_eq!(p.until_closed().await, Some(4000));
    let mut p2 = h.server.accept().await;
    p2.hello(41_250).await;
    assert_eq!(p2.recv_op().await["op"], 6);
    p2.send(json!({"op": 9, "d": true})).await;
    let mut p3 = h.server.accept().await;
    assert_eq!(p3.path, format!("/resume{QUERY}"));
    p3.hello(41_250).await;
    assert_eq!(p3.recv_op().await["op"], 6);
}

#[tokio::test]
async fn resume_after_drop_uses_resume_url() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.handshake(ready_json(&h.server, json!([]))).await;
    p.dispatch("SESSIONS_REPLACE", json!([])).await;
    drop(p); // the network drops: no close frame
    settle().await;
    assert_eq!(h.state.live.connection(), ConnState::Reconnecting);
    let mut p2 = h.server.accept().await;
    assert_eq!(p2.path, format!("/resume{QUERY}"));
    p2.hello(41_250).await;
    let resume = p2.recv_op().await;
    assert_eq!(resume["op"], 6);
    assert_eq!(resume["d"]["seq"], 2);
}

#[tokio::test]
async fn sessions_replace_ignored() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.handshake(ready_json(&h.server, json!([guild_json(10, 100, id_at(T - 60_000))]))).await;
    p.dispatch("SESSIONS_REPLACE", json!([{"session_id": "other", "status": "online"}])).await;
    p.dispatch("PRESENCE_UPDATE", json!({"user": {"id": "2"}})).await;
    p.dispatch("MESSAGE_CREATE", message_json(id_at(T - 1_000), 100, 10)).await;
    settle().await;
    assert!(matches!(h.state.live.connection(), ConnState::Ready { .. }));
    assert_eq!(h.state.live.channel(Id(100)).unwrap().last_message_id, Some(id_at(T - 1_000)));
    assert!(h.server.no_connection_within(300).await, "nothing reconnected");
}

#[tokio::test]
async fn socket_outlives_the_http_client_timeout() {
    // wreq's client has a 30 s request timeout. It must not reach the
    // upgraded WebSocket. Connect in real time, then pause the clock (wreq's
    // timer is tokio's) and hold the socket for 3 virtual minutes.
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    // A 10-minute interval: one unanswered beat at most, never a zombie close.
    p.hello(600_000).await;
    assert_eq!(p.recv_op().await["op"], 2);
    p.dispatch("READY", ready_json(&h.server, json!([guild_json(10, 100, id_at(T - 60_000))]))).await;
    settle().await;
    tokio::time::pause();
    let start = Instant::now();
    tokio::time::sleep(Duration::from_secs(180)).await;
    assert!(start.elapsed() >= Duration::from_secs(180));
    tokio::time::resume();
    p.dispatch("MESSAGE_CREATE", message_json(id_at(T - 1_000), 100, 10)).await;
    settle().await;
    assert_eq!(h.state.live.channel(Id(100)).unwrap().last_message_id, Some(id_at(T - 1_000)));
    assert!(matches!(h.state.live.connection(), ConnState::Ready { .. }));
    assert!(h.server.no_connection_within(100).await, "the socket was never replaced");
}

#[tokio::test]
async fn no_token_waits_for_login() {
    let h = harness(None).await;
    settle().await;
    assert_eq!(h.state.live.connection(), ConnState::Halted(DiscordError::NeedsLogin));
    assert!(h.server.no_connection_within(300).await);
    h.state.secrets.set(ACCOUNT_TOKEN, "tok-2").await.unwrap();
    assert_eq!(status(&h.state).await["connection"], "connecting");
    let mut p = h.server.accept().await;
    let identify = p.handshake(ready_json(&h.server, json!([]))).await;
    assert_eq!(identify["d"]["token"], "tok-2");
}

#[tokio::test]
async fn new_login_while_connected_reidentifies_with_new_token() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.handshake(ready_json(&h.server, json!([]))).await;
    settle().await;
    // A REST 401 kills tok-1 while the socket is still up; the user logs in again.
    h.state.live.reject_token("tok-1");
    h.state.secrets.set(ACCOUNT_TOKEN, "tok-2").await.unwrap();
    assert_eq!(status(&h.state).await["connection"], "connecting");
    assert_eq!(p.until_closed().await, Some(4000));
    let mut p2 = h.server.accept().await;
    assert_eq!(p2.path, QUERY);
    let identify = p2.handshake(ready_json(&h.server, json!([]))).await;
    assert_eq!(identify["d"]["token"], "tok-2");
    settle().await;
    assert_eq!(*h.opener.0.lock().unwrap(), vec!["tok-1".to_string(), "tok-2".to_string()]);
    assert!(h.state.ctx().is_ok());
}

#[tokio::test]
async fn missed_messages_after_reidentify_subscribe_the_guild() {
    let h = harness(Some("tok-1")).await;
    h.state.store.set_watch(Id(10), TargetKind::Guild, Some(WatchLevel::Full)).unwrap();
    let mut p = h.server.accept().await;
    let guilds = json!([guild_json(10, 100, id_at(T - 60_000)), guild_json(20, 200, id_at(T - 60_000))]);
    p.handshake(ready_json(&h.server, guilds)).await;
    p.dispatch("MESSAGE_CREATE", message_json(id_at(T - 30_000), 100, 10)).await;
    settle().await;
    assert_eq!(h.state.live.recent(Id(100)).len(), 1, "full-watch channels are buffered");
    assert!(h.state.live.recent(Id(200)).is_empty());

    p.send(json!({"op": 9, "d": false})).await;
    let mut p2 = h.server.accept().await;
    // G010's channel moved past what arrived live, before the drop: a miss.
    // G020 had nothing new.
    let guilds = json!([guild_json(10, 100, id_at(T - 10_000)), guild_json(20, 200, id_at(T - 60_000))]);
    p2.handshake(ready_json(&h.server, guilds)).await;
    let sub = p2.recv_op().await;
    assert_eq!(sub["op"], 37);
    let subs = sub["d"]["subscriptions"].as_object().unwrap();
    assert_eq!(subs.keys().collect::<Vec<_>>(), vec!["10"]);
    assert_eq!(subs["10"]["channels"], json!({"100": [[0, 99]]}));
    assert!(timeout(Duration::from_millis(300), p2.recv_op()).await.is_err(), "one op 37 only");
}

#[tokio::test]
async fn first_ready_sends_no_subscription() {
    let h = harness(Some("tok-1")).await;
    h.state.store.set_watch(Id(10), TargetKind::Guild, Some(WatchLevel::Full)).unwrap();
    let mut p = h.server.accept().await;
    p.handshake(ready_json(&h.server, json!([guild_json(10, 100, id_at(T - 10_000))]))).await;
    assert!(timeout(Duration::from_millis(300), p.recv_op()).await.is_err());
}

#[tokio::test]
async fn thread_list_sync_and_roles_reach_the_model() {
    let h = harness(Some("tok-1")).await;
    let mut p = h.server.accept().await;
    p.handshake(ready_json(&h.server, json!([guild_json(10, 100, id_at(T - 60_000))]))).await;
    p.dispatch(
        "THREAD_LIST_SYNC",
        json!({"guild_id": "10", "threads": [{"id": "110", "type": 11, "parent_id": "100"}]}),
    )
    .await;
    p.dispatch("GUILD_ROLE_CREATE", json!({"guild_id": "10", "role": {"id": "11", "permissions": "0"}})).await;
    settle().await;
    assert_eq!(h.state.live.channel(Id(110)).unwrap().guild_id, Some(Id(10)));
    assert!(h.state.live.guild(Id(10)).unwrap().roles.iter().any(|r| r.id == Id(11)));
}
