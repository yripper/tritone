//! `HttpDiscord`: the real `DiscordApi`, over the Chrome-emulating wreq client.
//!
//! Every call goes through one `Pacer` (one request in flight, gaps between
//! requests) and one halt slot: once Discord answers with something that
//! needs the user (a dead token, a CAPTCHA, a locked account, an edge block),
//! every later call returns that error without touching the network, and the
//! halt is reported to `LiveState` so tools and `status` see it too.
//!
//! Status handling follows https://docs.discord.food/topics/rate-limits and
//! https://docs.discord.food/topics/captcha-handling.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Map, Value, json};
use url::Url;

use super::api::{DResult, DiscordApi};
use super::fingerprint::Fingerprint;
use super::parse::{parse_channel, parse_message, parse_user};
use super::rate::{Pacer, ceil_secs};
use super::transport::{CookieJar, http_client};
use super::types::{
    Channel, DiscordError, Id, Message, MuteTarget, OutgoingMessage, SearchPage, SearchQuery, SearchScope, UserRef,
};
use crate::domain::live::{ConnState, LiveState};
use crate::secrets::SecretStore;

pub const API_BASE: &str = "https://discord.com/api/v9";
/// A GET is retried once after a 429 only when Discord asks for at most this long.
const MAX_GET_RETRY_WAIT: Duration = Duration::from_secs(10);
/// An edge block keeps tritone away at least this long.
const MIN_BLOCK_S: u64 = 60;
/// Search answers 202 while indexing; `retry_after` 0 means "soon".
const DEFAULT_INDEXING_S: u64 = 5;
/// Discord's own bulk-ack chunk size.
const ACK_BULK_CHUNK: usize = 100;
const SUPPRESS_NOTIFICATIONS: u32 = 1 << 12;
const DAY_MS: u64 = 86_400_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    fn is_write(self) -> bool {
        self != Method::Get
    }

    fn wreq(self) -> wreq::Method {
        match self {
            Method::Get => wreq::Method::GET,
            Method::Post => wreq::Method::POST,
            Method::Put => wreq::Method::PUT,
            Method::Patch => wreq::Method::PATCH,
            Method::Delete => wreq::Method::DELETE,
        }
    }
}

/// One request: the path as segments (each percent-encoded on its own).
struct Req {
    method: Method,
    path: Vec<String>,
    query: Vec<(&'static str, String)>,
    body: Option<Value>,
    extra: Vec<(&'static str, String)>,
}

impl Req {
    fn new(method: Method, path: &[&str]) -> Req {
        Req {
            method,
            path: path.iter().map(|s| s.to_string()).collect(),
            query: Vec::new(),
            body: None,
            extra: Vec::new(),
        }
    }

    fn query(mut self, name: &'static str, value: impl ToString) -> Self {
        self.query.push((name, value.to_string()));
        self
    }

    fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    fn header(mut self, name: &'static str, value: String) -> Self {
        self.extra.push((name, value));
        self
    }
}

/// A successful answer: status and JSON body (`Null` when empty).
struct Answer {
    status: u16,
    body: Value,
}

pub struct HttpDiscord {
    base: Url,
    token: String,
    fp: Arc<Fingerprint>,
    client: wreq::Client,
    live: Arc<LiveState>,
    halt: Mutex<Option<DiscordError>>,
    pacer: Pacer,
}

impl HttpDiscord {
    /// `base` is the API root (`API_BASE`; a mock server in tests). `live`
    /// receives halts: a 401 rejects `token`, a CAPTCHA, lock or block halts.
    pub fn new(
        base: Url,
        token: String,
        fp: Arc<Fingerprint>,
        client: wreq::Client,
        live: Arc<LiveState>,
    ) -> HttpDiscord {
        HttpDiscord { base, token, fp, client, live, halt: Mutex::new(None), pacer: Pacer::new() }
    }

    /// Why no call may reach Discord now: this client's own halt, or one
    /// the gateway or another client reported to `LiveState`.
    fn stopped(&self) -> Option<DiscordError> {
        let own = self.halt.lock().unwrap_or_else(|e| e.into_inner()).clone();
        own.or_else(|| match self.live.connection() {
            ConnState::Halted(e) => Some(e),
            _ => None,
        })
    }

    /// Records a halting error here and in `LiveState`. The first halt wins.
    fn set_halt(&self, e: &DiscordError) {
        {
            let mut slot = self.halt.lock().unwrap_or_else(|p| p.into_inner());
            if slot.is_some() {
                return;
            }
            *slot = Some(e.clone());
        }
        tracing::warn!(kind = e.kind(), "Discord REST halted");
        match e {
            DiscordError::NeedsLogin => self.live.reject_token(&self.token),
            other => self.live.halt(other.clone()),
        }
    }

    fn url(&self, req: &Req) -> DResult<Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| DiscordError::Http("the API base URL can't take a path".into()))?
            .pop_if_empty()
            .extend(&req.path);
        if !req.query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in &req.query {
                q.append_pair(k, v);
            }
        }
        Ok(url)
    }

    /// Browser headers, with `Authorization` (and the request's own) after
    /// `Sec-Ch-Ua-Platform`, where Discord web's fetch puts them.
    fn headers(&self, req: &Req) -> Vec<(&'static str, String)> {
        let mut base = self.fp.rest_headers(req.method.is_write()).into_iter();
        let mut h: Vec<(&'static str, String)> = base.by_ref().take(1).collect();
        h.push(("Authorization", self.token.clone()));
        if req.body.is_some() {
            h.push(("Content-Type", "application/json".into()));
        }
        h.extend(req.extra.iter().cloned());
        h.extend(base);
        h
    }

    async fn call(&self, req: Req) -> DResult<Answer> {
        if let Some(e) = self.stopped() {
            return Err(e);
        }
        let mut turn =
            self.pacer.turn(req.method.is_write()).await.map_err(|s| DiscordError::RateLimited { retry_after_s: s })?;
        // Calls queue behind the one in flight; if that one halted, they stop here.
        if let Some(e) = self.stopped() {
            return Err(e);
        }
        let url = self.url(&req)?;
        let mut retried = false;
        loop {
            let raw = self.send_once(&req, url.clone()).await?;
            match classify(raw) {
                Outcome::Ok(a) => return Ok(a),
                Outcome::RateLimited(wait) => {
                    turn.cool_down(wait);
                    if req.method == Method::Get && !retried && wait <= MAX_GET_RETRY_WAIT {
                        retried = true;
                        tracing::info!(wait_ms = wait.as_millis() as u64, "Discord 429; retrying the read once");
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(DiscordError::RateLimited { retry_after_s: ceil_secs(wait) });
                }
                Outcome::Err(e) => {
                    if let DiscordError::Blocked { retry_after_s } = e {
                        // The block's window outlives the halt: if the user clears
                        // the halt early, calls still get `RateLimited` without a
                        // request until it has passed.
                        turn.cool_down(Duration::from_secs(retry_after_s));
                    }
                    if e.halts() {
                        self.set_halt(&e);
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn send_once(&self, req: &Req, url: Url) -> DResult<Raw> {
        let mut rb = self.client.request(req.method.wreq(), url.as_str());
        for (name, value) in self.headers(req) {
            rb = rb.header(name, value);
        }
        if let Some(body) = &req.body {
            rb = rb.body(body.to_string());
        }
        let resp = rb.send().await.map_err(|e| DiscordError::Http(without_url(&e)))?;
        let status = resp.status().as_u16();
        let header = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        let content_type = header("content-type").unwrap_or_default();
        let retry_after = header("retry-after").and_then(|v| v.trim().parse::<f64>().ok());
        let bytes = resp.bytes().await.map_err(|e| DiscordError::Http(without_url(&e)))?;
        Ok(Raw { status, content_type, retry_after, bytes: bytes.to_vec() })
    }

    async fn messages(&self, req: Req) -> DResult<Vec<Message>> {
        let a = self.call(req).await?;
        Ok(a.body.as_array().map(|arr| arr.iter().filter_map(parse_message).collect()).unwrap_or_default())
    }
}

/// wreq errors name the URL, which carries ids; the message goes back to Claude, not logs, but keep it short.
fn without_url(e: &wreq::Error) -> String {
    let kind = if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "could not connect"
    } else {
        "request failed"
    };
    kind.to_string()
}

struct Raw {
    status: u16,
    content_type: String,
    /// The `Retry-After` header, in seconds.
    retry_after: Option<f64>,
    bytes: Vec<u8>,
}

enum Outcome {
    Ok(Answer),
    RateLimited(Duration),
    Err(DiscordError),
}

/// Maps an answer to success, a 429 wait, or an error. Halting errors per
/// https://docs.discord.food/topics/rate-limits#cloudflare-bans and
/// https://docs.discord.food/topics/captcha-handling.
fn classify(raw: Raw) -> Outcome {
    let json: Option<Value> =
        if raw.bytes.is_empty() { Some(Value::Null) } else { serde_json::from_slice(&raw.bytes).ok() };
    let is_json = json.is_some() && (raw.bytes.is_empty() || raw.content_type.contains("json"));
    let body = json.unwrap_or(Value::Null);
    let code = body.get("code").and_then(Value::as_u64).map(|c| c as u32);
    let block_s = raw.retry_after.map_or(MIN_BLOCK_S, |s| (s.ceil() as u64).max(MIN_BLOCK_S));
    let blocked = DiscordError::Blocked { retry_after_s: block_s };

    if (200..300).contains(&raw.status) {
        return Outcome::Ok(Answer { status: raw.status, body });
    }
    if matches!(code, Some(40002 | 40004)) {
        return Outcome::Err(DiscordError::Locked);
    }
    if code == Some(40333) {
        return Outcome::Err(blocked);
    }
    match raw.status {
        400 if body.get("captcha_key").is_some() => Outcome::Err(DiscordError::Captcha),
        // 60003: the action needs MFA, a security challenge, not a dead token
        // (https://docs.discord.food/topics/opcodes-and-status-codes#json-error-codes).
        401 if code == Some(60003) => Outcome::Err(DiscordError::Locked),
        401 => Outcome::Err(DiscordError::NeedsLogin),
        403 | 429 if !is_json => Outcome::Err(blocked),
        403 => Outcome::Err(DiscordError::Forbidden),
        404 => Outcome::Err(DiscordError::NotFound),
        429 => {
            let secs = body.get("retry_after").and_then(Value::as_f64).or(raw.retry_after);
            match secs {
                Some(s) if s.is_finite() && s >= 0.0 => Outcome::RateLimited(Duration::from_secs_f64(s)),
                // No wait given: never retry blind.
                _ => Outcome::Err(blocked),
            }
        }
        status => Outcome::Err(DiscordError::Api { status, code }),
    }
}

/// Days since 2015-01-01, Discord's `last_viewed`.
fn days_since_discord_epoch(now_ms: i64) -> u64 {
    (Id::from_timestamp_ms(now_ms, 0).0 >> 22) / DAY_MS
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `name:id` for a custom emoji (also accepts `<:name:id>` and `<a:name:id>`),
/// the emoji itself otherwise.
fn reaction_path(emoji: &str) -> String {
    let e = emoji.trim();
    let inner = e.strip_prefix('<').and_then(|s| s.strip_suffix('>')).unwrap_or(e);
    let inner = inner.strip_prefix("a:").unwrap_or(inner);
    let inner = inner.strip_prefix(':').unwrap_or(inner);
    inner.to_string()
}

fn search_hits(body: &Value) -> Vec<Message> {
    let groups = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    groups
        .iter()
        .filter_map(|g| match g {
            Value::Array(ctx) => ctx.iter().find(|m| m.get("hit") == Some(&Value::Bool(true))).or(ctx.first()),
            m @ Value::Object(_) => Some(m),
            _ => None,
        })
        .filter_map(parse_message)
        .collect()
}

#[async_trait]
impl DiscordApi for HttpDiscord {
    async fn history(&self, channel: Id, before: Option<Id>, after: Option<Id>, limit: u8) -> DResult<Vec<Message>> {
        let mut req = Req::new(Method::Get, &["channels", &channel.to_string(), "messages"]);
        if let Some(b) = before {
            req = req.query("before", b);
        } else if let Some(a) = after {
            req = req.query("after", a);
        }
        self.messages(req.query("limit", limit.clamp(1, 100))).await
    }

    async fn around(&self, channel: Id, message: Id, limit: u8) -> DResult<Vec<Message>> {
        let req = Req::new(Method::Get, &["channels", &channel.to_string(), "messages"])
            .query("around", message)
            .query("limit", limit.clamp(1, 100));
        self.messages(req).await
    }

    async fn recent_mentions(&self, limit: u8, before: Option<Id>) -> DResult<Vec<Message>> {
        let mut req = Req::new(Method::Get, &["users", "@me", "mentions"]);
        if let Some(b) = before {
            req = req.query("before", b);
        }
        let req = req.query("limit", limit.clamp(1, 100)).query("roles", "true").query("everyone", "true");
        self.messages(req).await
    }

    async fn search(&self, scope: SearchScope, query: &SearchQuery) -> DResult<SearchPage> {
        let mut req = match scope {
            SearchScope::Guild(g) => Req::new(Method::Get, &["guilds", &g.to_string(), "messages", "search"]),
            SearchScope::Channel(c) => Req::new(Method::Get, &["channels", &c.to_string(), "messages", "search"]),
        };
        if let Some(c) = &query.content {
            req = req.query("content", c);
        }
        if let Some(a) = query.author_id {
            req = req.query("author_id", a);
        }
        if let Some(c) = query.channel_id {
            req = req.query("channel_id", c);
        }
        if let Some(m) = query.min_id {
            req = req.query("min_id", m);
        }
        if let Some(m) = query.max_id {
            req = req.query("max_id", m);
        }
        if query.offset > 0 {
            req = req.query("offset", query.offset);
        }
        let a = self.call(req).await?;
        if a.status == 202 {
            let s = a.body.get("retry_after").and_then(Value::as_f64).unwrap_or(0.0);
            let s = if s > 0.0 { s.ceil() as u64 } else { DEFAULT_INDEXING_S };
            return Err(DiscordError::Indexing { retry_after_s: s });
        }
        let total = a.body.get("total_results").and_then(Value::as_u64).unwrap_or(0) as u32;
        Ok(SearchPage { total, messages: search_hits(&a.body) })
    }

    async fn ack(&self, channel: Id, message: Id) -> DResult<()> {
        let body = json!({"token": null, "last_viewed": days_since_discord_epoch(now_ms())});
        let req = Req::new(Method::Post, &["channels", &channel.to_string(), "messages", &message.to_string(), "ack"]);
        self.call(req.body(body)).await.map(drop)
    }

    async fn ack_bulk(&self, items: &[(Id, Id)]) -> DResult<()> {
        for chunk in items.chunks(ACK_BULK_CHUNK) {
            let read_states: Vec<Value> =
                chunk.iter().map(|(c, m)| json!({"read_state_type": 0, "channel_id": c, "message_id": m})).collect();
            let req = Req::new(Method::Post, &["read-states", "ack-bulk"]).body(json!({"read_states": read_states}));
            self.call(req).await?;
        }
        Ok(())
    }

    async fn typing(&self, channel: Id) -> DResult<()> {
        self.call(Req::new(Method::Post, &["channels", &channel.to_string(), "typing"])).await.map(drop)
    }

    async fn send(&self, channel: Id, msg: &OutgoingMessage) -> DResult<Message> {
        let mut body = Map::new();
        body.insert("content".into(), json!(msg.content));
        body.insert("nonce".into(), json!(msg.nonce));
        body.insert("tts".into(), json!(false));
        body.insert("flags".into(), json!(if msg.silent { SUPPRESS_NOTIFICATIONS } else { 0 }));
        body.insert("mobile_network_type".into(), json!("unknown"));
        if let Some(reply) = msg.reply_to {
            let mut r = Map::new();
            r.insert("message_id".into(), json!(reply));
            r.insert("channel_id".into(), json!(channel));
            if let Some(g) = self.live.channel(channel).and_then(|c| c.guild_id) {
                r.insert("guild_id".into(), json!(g));
            }
            r.insert("fail_if_not_exists".into(), json!(false));
            body.insert("message_reference".into(), Value::Object(r));
        }
        let ctx = base64::engine::general_purpose::STANDARD.encode(r#"{"location":"chat_input"}"#);
        let req = Req::new(Method::Post, &["channels", &channel.to_string(), "messages"])
            .header("X-Context-Properties", ctx)
            .body(Value::Object(body));
        let a = self.call(req).await?;
        parse_message(&a.body).ok_or(DiscordError::Api { status: a.status, code: None })
    }

    async fn react(&self, channel: Id, message: Id, emoji: &str) -> DResult<()> {
        let path = [
            "channels",
            &channel.to_string(),
            "messages",
            &message.to_string(),
            "reactions",
            &reaction_path(emoji),
            "@me",
        ];
        self.call(Req::new(Method::Put, &path)).await.map(drop)
    }

    async fn create_thread(&self, channel: Id, from_message: Option<Id>, name: &str) -> DResult<Channel> {
        let req = match from_message {
            Some(m) => {
                Req::new(Method::Post, &["channels", &channel.to_string(), "messages", &m.to_string(), "threads"])
                    .body(json!({"name": name, "auto_archive_duration": 1440}))
            }
            None => Req::new(Method::Post, &["channels", &channel.to_string(), "threads"])
                .body(json!({"name": name, "type": 11, "auto_archive_duration": 1440})),
        };
        let a = self.call(req).await?;
        parse_channel(&a.body).ok_or(DiscordError::Api { status: a.status, code: None })
    }

    /// The forum's first-message object has no nonce field
    /// (https://docs.discord.food/resources/channel#create-thread), so
    /// `_nonce` isn't sent: a post is never retried here.
    async fn create_forum_post(&self, forum: Id, name: &str, content: &str, _nonce: Id) -> DResult<(Channel, Message)> {
        let body = json!({"name": name, "applied_tags": [], "message": {"content": content}});
        let a = self.call(Req::new(Method::Post, &["channels", &forum.to_string(), "threads"]).body(body)).await?;
        let bad = DiscordError::Api { status: a.status, code: None };
        let channel = parse_channel(&a.body).ok_or(bad.clone())?;
        let message = a.body.get("message").and_then(parse_message).ok_or(bad)?;
        Ok((channel, message))
    }

    async fn leave_guild(&self, guild: Id) -> DResult<()> {
        let req =
            Req::new(Method::Delete, &["users", "@me", "guilds", &guild.to_string()]).body(json!({"lurking": false}));
        self.call(req).await.map(drop)
    }

    async fn set_mute(&self, target: MuteTarget, muted: bool) -> DResult<()> {
        let config = if muted { json!({"end_time": null, "selected_time_window": -1}) } else { Value::Null };
        let (guild, body) = match target {
            MuteTarget::Guild(g) => (g.to_string(), json!({"muted": muted, "mute_config": config})),
            MuteTarget::Channel { guild, channel } => {
                let mut overrides = Map::new();
                overrides.insert(channel.to_string(), json!({"muted": muted, "mute_config": config}));
                (guild.map_or_else(|| "@me".to_string(), |g| g.to_string()), json!({"channel_overrides": overrides}))
            }
        };
        let req = Req::new(Method::Patch, &["users", "@me", "guilds", &guild, "settings"]).body(body);
        self.call(req).await.map(drop)
    }

    async fn user(&self, user: Id) -> DResult<UserRef> {
        let req = Req::new(Method::Get, &["users", &user.to_string(), "profile"]).query("with_mutual_guilds", "false");
        let a = self.call(req).await?;
        let u = a.body.get("user").unwrap_or(&a.body);
        parse_user(u).ok_or(DiscordError::Api { status: a.status, code: None })
    }

    /// The user's explicit OK after a CAPTCHA, lock or block (through
    /// `SendQueue::resume`, with `LiveState::clear_halt`). A dead token stays
    /// halted: only a new login, which builds a new client, lifts that.
    fn clear_halt(&self) {
        let mut slot = self.halt.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|e| *e != DiscordError::NeedsLogin) {
            tracing::info!(kind = slot.as_ref().map_or("none", DiscordError::kind), "REST halt cleared by the user");
            *slot = None;
        }
    }
}

/// The REST client for `token`, after reloading `jar` from the keyring (a
/// `tritone login` may have just written it). Call on the first connect and
/// whenever a new login wakes the daemon. Returns the client too, so the
/// gateway's WebSocket uses the same TLS stack and cookies.
pub async fn rebuild_api(
    jar: &Arc<CookieJar>,
    secrets: &dyn SecretStore,
    fp: Arc<Fingerprint>,
    live: Arc<LiveState>,
    token: String,
) -> Result<(Arc<dyn DiscordApi>, wreq::Client)> {
    jar.reload(secrets).await?;
    let client = http_client(jar.clone())?;
    let api = HttpDiscord::new(Url::parse(API_BASE)?, token, fp, client.clone(), live);
    Ok((Arc::new(api), client))
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
