use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::HttpDiscord;
use crate::discord::DiscordApi;
use crate::discord::fingerprint::Fingerprint;
use crate::discord::transport::{CookieJar, http_client};
use crate::discord::types::{DiscordError, Id, MuteTarget, OutgoingMessage, SearchQuery, SearchScope};
use crate::domain::live::{BufferPolicy, ConnState, LiveState};

const TOKEN: &str = "fake-token-0001";

struct Env {
    server: MockServer,
    live: Arc<LiveState>,
    api: HttpDiscord,
}

async fn env() -> Env {
    let server = MockServer::start().await;
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 10, max_age_ms: 3_600_000 }));
    let client = http_client(Arc::new(CookieJar::empty())).unwrap();
    let base = url::Url::parse(&format!("{}/api/v9", server.uri())).unwrap();
    let api = HttpDiscord::new(base, TOKEN.into(), Arc::new(Fingerprint::for_this_machine()), client, live.clone());
    Env { server, live, api }
}

fn message_json(id: u64, channel: u64) -> Value {
    json!({
        "id": id.to_string(), "channel_id": channel.to_string(), "type": 0, "content": "hi",
        "author": {"id": "2", "username": "U002"},
        "timestamp": "2026-01-02T03:04:05+00:00", "edited_timestamp": null,
        "mentions": [], "mention_roles": [], "attachments": [], "embeds": []
    })
}

fn body(req: &Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
}

fn header_of<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers.get(name).and_then(|v| v.to_str().ok())
}

async fn received(env: &Env) -> Vec<Request> {
    env.server.received_requests().await.unwrap()
}

#[tokio::test]
async fn history_sends_super_properties_and_parses() {
    let env = env().await;
    Mock::given(method("GET"))
        .and(path("/api/v9/channels/101/messages"))
        .and(query_param("limit", "50"))
        .and(query_param("before", "900"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([message_json(1002, 101), message_json(1001, 101)])),
        )
        .expect(1)
        .mount(&env.server)
        .await;
    let msgs = env.api.history(Id(101), Some(Id(900)), None, 50).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![Id(1002), Id(1001)]);

    let req = &received(&env).await[0];
    assert_eq!(header_of(req, "authorization"), Some(TOKEN));
    let sp = header_of(req, "x-super-properties").expect("super-properties sent");
    let decoded: Value =
        serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(sp).unwrap()).unwrap();
    assert_eq!(decoded["browser"], "Chrome");
    assert!(header_of(req, "user-agent").unwrap().contains("Chrome/"));
    // Reads carry no Origin, like the browser's same-origin GETs.
    assert_eq!(header_of(req, "origin"), None);
}

#[tokio::test]
async fn send_reply_body_has_reference_and_nonce() {
    let env = env().await;
    Mock::given(method("POST"))
        .and(path("/api/v9/channels/101/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message_json(5555, 101)))
        .expect(1)
        .mount(&env.server)
        .await;
    let msg = OutgoingMessage { content: "hello".into(), reply_to: Some(Id(900)), nonce: Id(4242), silent: true };
    let sent = env.api.send(Id(101), &msg).await.unwrap();
    assert_eq!(sent.id, Id(5555));

    let req = &received(&env).await[0];
    let b = body(req);
    assert_eq!(
        b,
        json!({
            "content": "hello", "nonce": "4242", "tts": false, "flags": 4096, "mobile_network_type": "unknown",
            "message_reference": {"message_id": "900", "channel_id": "101", "fail_if_not_exists": false}
        })
    );
    let keys: Vec<&String> = b.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["content", "nonce", "tts", "flags", "mobile_network_type", "message_reference"]);
    let ctx = header_of(req, "x-context-properties").expect("context properties");
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(ctx).unwrap(), br#"{"location":"chat_input"}"#);
    assert_eq!(header_of(req, "origin"), Some("https://discord.com"));
    assert_eq!(header_of(req, "content-type"), Some("application/json"));
}

#[tokio::test]
async fn http_401_halts_all_calls() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"message": "401: Unauthorized", "code": 0})))
        .mount(&env.server)
        .await;
    assert_eq!(env.api.history(Id(101), None, None, 50).await, Err(DiscordError::NeedsLogin));
    assert_eq!(env.api.around(Id(101), Id(5), 10).await, Err(DiscordError::NeedsLogin));
    assert_eq!(env.api.typing(Id(101)).await, Err(DiscordError::NeedsLogin));
    assert_eq!(received(&env).await.len(), 1, "after a 401 nothing reaches the network");
}

#[tokio::test]
async fn http_401_rejects_token_in_live() {
    let env = env().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&env.server).await;
    let _ = env.api.history(Id(101), None, None, 50).await;
    assert_eq!(env.live.connection(), ConnState::Halted(DiscordError::NeedsLogin));
    // The dead token is remembered: finding it again in the keyring doesn't wake anything.
    assert!(!env.live.try_login_with(TOKEN));
    assert!(env.live.try_login_with("fake-token-0002"));
}

#[tokio::test]
async fn captcha_400_halts_with_captcha() {
    let env = env().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "captcha_key": ["captcha-required"], "captcha_sitekey": "x", "captcha_service": "hcaptcha"
        })))
        .mount(&env.server)
        .await;
    let msg = OutgoingMessage { content: "x".into(), reply_to: None, nonce: Id(1), silent: false };
    assert_eq!(env.api.send(Id(101), &msg).await, Err(DiscordError::Captcha));
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::Captcha));
    assert_eq!(received(&env).await.len(), 1);
    assert_eq!(env.live.connection(), ConnState::Halted(DiscordError::Captcha));
}

#[tokio::test]
async fn html_403_is_blocked() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("content-type", "text/html")
                .set_body_string("<html>Access denied</html>"),
        )
        .mount(&env.server)
        .await;
    let e = env.api.history(Id(101), None, None, 5).await.unwrap_err();
    assert_eq!(e, DiscordError::Blocked { retry_after_s: 60 });
    assert_eq!(env.live.connection(), ConnState::Halted(e));
}

#[tokio::test]
async fn json_403_is_forbidden_and_does_not_halt() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": "Missing Access", "code": 50001})))
        .mount(&env.server)
        .await;
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::Forbidden));
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::Forbidden));
    assert_eq!(received(&env).await.len(), 2);
}

#[tokio::test]
async fn locked_and_40333_codes_halt() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message": "verify", "code": 40002})))
        .mount(&env.server)
        .await;
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::Locked));

    let other = self::env().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(json!({"message": "internal network error", "code": 40333})),
        )
        .mount(&other.server)
        .await;
    assert_eq!(other.api.history(Id(101), None, None, 5).await, Err(DiscordError::Blocked { retry_after_s: 60 }));
}

#[tokio::test]
async fn retry_after_honored_once_for_get() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "1")
                .set_body_json(json!({"message": "You are being rate limited.", "retry_after": 0.4, "global": false})),
        )
        .up_to_n_times(1)
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([message_json(1, 101)])))
        .mount(&env.server)
        .await;
    let t0 = Instant::now();
    let msgs = env.api.history(Id(101), None, None, 5).await.unwrap();
    assert_eq!(msgs.len(), 1);
    assert!(t0.elapsed() >= Duration::from_millis(390), "waited retry_after before retrying");
    assert_eq!(received(&env).await.len(), 2);
}

#[tokio::test]
async fn get_429_retried_only_once_then_rate_limited() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"message": "slow", "retry_after": 0.05, "global": false})),
        )
        .mount(&env.server)
        .await;
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::RateLimited { retry_after_s: 1 }));
    assert_eq!(received(&env).await.len(), 2);
}

#[tokio::test]
async fn long_get_429_is_not_retried_and_cools_down() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"message": "slow", "retry_after": 30.0, "global": true})),
        )
        .mount(&env.server)
        .await;
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::RateLimited { retry_after_s: 30 }));
    // During the cooldown nothing goes out.
    assert!(matches!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::RateLimited { .. })));
    assert_eq!(received(&env).await.len(), 1);
    // A 429 isn't a halt.
    assert!(!matches!(env.live.connection(), ConnState::Halted(_)));
}

#[tokio::test]
async fn post_429_not_retried() {
    let env = env().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"message": "slow", "retry_after": 0.05, "global": false})),
        )
        .mount(&env.server)
        .await;
    let msg = OutgoingMessage { content: "x".into(), reply_to: None, nonce: Id(1), silent: false };
    assert_eq!(env.api.send(Id(101), &msg).await, Err(DiscordError::RateLimited { retry_after_s: 1 }));
    assert_eq!(received(&env).await.len(), 1);
}

#[tokio::test]
async fn a_429_without_retry_after_is_blocked() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({"message": "slow"})))
        .mount(&env.server)
        .await;
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::Blocked { retry_after_s: 60 }));
    assert_eq!(received(&env).await.len(), 1);
}

#[tokio::test]
async fn search_202_is_indexing() {
    let env = env().await;
    Mock::given(method("GET"))
        .and(path("/api/v9/guilds/11/messages/search"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json!({"code": 110000, "retry_after": 0})))
        .mount(&env.server)
        .await;
    let q = SearchQuery { author_id: Some(Id(1)), ..Default::default() };
    assert_eq!(env.api.search(SearchScope::Guild(Id(11)), &q).await, Err(DiscordError::Indexing { retry_after_s: 5 }));
}

#[tokio::test]
async fn search_returns_hits_and_total() {
    let env = env().await;
    let mut hit = message_json(1002, 101);
    hit["hit"] = json!(true);
    Mock::given(method("GET"))
        .and(path("/api/v9/channels/101/messages/search"))
        .and(query_param("author_id", "1"))
        .and(query_param("offset", "25"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"total_results": 40, "messages": [[message_json(1001, 101), hit]]})),
        )
        .mount(&env.server)
        .await;
    let q = SearchQuery { author_id: Some(Id(1)), offset: 25, ..Default::default() };
    let page = env.api.search(SearchScope::Channel(Id(101)), &q).await.unwrap();
    assert_eq!(page.total, 40);
    assert_eq!(page.messages.iter().map(|m| m.id).collect::<Vec<_>>(), vec![Id(1002)]);
}

#[tokio::test]
async fn ack_body_shape() {
    let env = env().await;
    Mock::given(method("POST"))
        .and(path("/api/v9/channels/101/messages/1005/ack"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": null})))
        .expect(1)
        .mount(&env.server)
        .await;
    env.api.ack(Id(101), Id(1005)).await.unwrap();
    let b = body(&received(&env).await[0]);
    assert_eq!(b["token"], Value::Null);
    let days = b["last_viewed"].as_u64().unwrap();
    // Days since 2015-01-01: past 2026-01-01 (4018 days).
    assert!(days > 4018, "{days}");
}

#[tokio::test]
async fn ack_bulk_body_shape() {
    let env = env().await;
    Mock::given(method("POST"))
        .and(path("/api/v9/read-states/ack-bulk"))
        .and(body_json(json!({"read_states": [
            {"read_state_type": 0, "channel_id": "101", "message_id": "1005"},
            {"read_state_type": 0, "channel_id": "102", "message_id": "1006"}
        ]})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&env.server)
        .await;
    env.api.ack_bulk(&[(Id(101), Id(1005)), (Id(102), Id(1006))]).await.unwrap();
}

#[tokio::test]
async fn mute_channel_patch_uses_map() {
    let env = env().await;
    Mock::given(method("PATCH"))
        .and(path("/api/v9/users/@me/guilds/11/settings"))
        .and(body_json(json!({"channel_overrides": {"101": {
            "muted": true, "mute_config": {"end_time": null, "selected_time_window": -1}
        }}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v9/users/@me/guilds/@me/settings"))
        .and(body_json(json!({"channel_overrides": {"301": {"muted": false, "mute_config": null}}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v9/users/@me/guilds/12/settings"))
        .and(body_json(json!({"muted": true, "mute_config": {"end_time": null, "selected_time_window": -1}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&env.server)
        .await;
    env.api.set_mute(MuteTarget::Channel { guild: Some(Id(11)), channel: Id(101) }, true).await.unwrap();
    env.api.set_mute(MuteTarget::Channel { guild: None, channel: Id(301) }, false).await.unwrap();
    env.api.set_mute(MuteTarget::Guild(Id(12)), true).await.unwrap();
}

#[tokio::test]
async fn react_encodes_the_emoji() {
    let env = env().await;
    Mock::given(method("PUT")).respond_with(ResponseTemplate::new(204)).mount(&env.server).await;
    env.api.react(Id(101), Id(1005), "👍").await.unwrap();
    env.api.react(Id(101), Id(1005), "<:party:77>").await.unwrap();
    let reqs = received(&env).await;
    assert_eq!(reqs[0].url.path(), "/api/v9/channels/101/messages/1005/reactions/%F0%9F%91%8D/@me");
    assert_eq!(reqs[1].url.path(), "/api/v9/channels/101/messages/1005/reactions/party:77/@me");
}

#[tokio::test]
async fn threads_forum_posts_leave_and_user() {
    let env = env().await;
    let thread = json!({"id": "1005", "type": 11, "guild_id": "11", "parent_id": "101", "name": "T001"});
    Mock::given(method("POST"))
        .and(path("/api/v9/channels/101/messages/1005/threads"))
        .and(body_json(json!({"name": "T001", "auto_archive_duration": 1440})))
        .respond_with(ResponseTemplate::new(201).set_body_json(thread.clone()))
        .mount(&env.server)
        .await;
    let mut post = json!({"id": "2001", "type": 11, "guild_id": "11", "parent_id": "150", "name": "P001"});
    post["message"] = message_json(2001, 2001);
    Mock::given(method("POST"))
        .and(path("/api/v9/channels/150/threads"))
        .and(body_json(json!({"name": "P001", "applied_tags": [], "message": {"content": "body"}})))
        .respond_with(ResponseTemplate::new(201).set_body_json(post))
        .mount(&env.server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/api/v9/users/@me/guilds/11"))
        .and(body_json(json!({"lurking": false})))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v9/users/2/profile"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"user": {"id": "2", "username": "U002", "global_name": "Two"}})),
        )
        .mount(&env.server)
        .await;

    let t = env.api.create_thread(Id(101), Some(Id(1005)), "T001").await.unwrap();
    assert_eq!((t.id, t.parent_id), (Id(1005), Some(Id(101))));
    let (c, m) = env.api.create_forum_post(Id(150), "P001", "body", Id(9)).await.unwrap();
    assert_eq!((c.id, m.id), (Id(2001), Id(2001)));
    env.api.leave_guild(Id(11)).await.unwrap();
    let u = env.api.user(Id(2)).await.unwrap();
    assert_eq!(u.label(), "Two");
}

#[tokio::test]
async fn mentions_query() {
    let env = env().await;
    Mock::given(method("GET"))
        .and(path("/api/v9/users/@me/mentions"))
        .and(query_param("limit", "25"))
        .and(query_param("roles", "true"))
        .and(query_param("everyone", "true"))
        .and(header("authorization", TOKEN))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([message_json(1, 101)])))
        .expect(1)
        .mount(&env.server)
        .await;
    assert_eq!(env.api.recent_mentions(25, None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn writes_are_paced_half_a_second_apart() {
    let env = env().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(204)).mount(&env.server).await;
    let t0 = Instant::now();
    env.api.typing(Id(101)).await.unwrap();
    env.api.typing(Id(101)).await.unwrap();
    assert!(t0.elapsed() >= Duration::from_millis(495));
}

fn discord_cookies(jar: &CookieJar) -> Option<String> {
    use wreq::cookie::{CookieStore, Cookies};
    let uri: wreq::Uri = "https://discord.com/api/v9/users/@me".parse().unwrap();
    match jar.cookies(&uri, wreq::Version::HTTP_11) {
        Cookies::Compressed(v) => Some(v.to_str().unwrap().to_string()),
        _ => None,
    }
}

#[tokio::test]
async fn rebuild_api_reloads_the_cookies_a_login_saved() {
    use crate::secrets::SecretStore;
    use crate::secrets::fake::MemSecrets;
    use wreq::cookie::CookieStore;

    let secrets = MemSecrets::default();
    // The daemon's jar, with an older cookie it hasn't saved yet.
    let daemon_jar = Arc::new(CookieJar::empty());
    let set = |jar: &CookieJar, c: &str| {
        let v = wreq::header::HeaderValue::from_str(c).unwrap();
        jar.set_cookies(&mut std::iter::once(&v), &"https://discord.com/".parse().unwrap());
    };
    set(&daemon_jar, "__dcfduid=old; Path=/; Max-Age=3600; Secure");
    // `tritone login`, in another process, saves its own jar.
    let login_jar = CookieJar::empty();
    set(&login_jar, "__dcfduid=new; Path=/; Max-Age=3600; Secure");
    assert!(login_jar.save_if_dirty(&secrets, 0).await.unwrap());
    let saved = secrets.get(crate::secrets::ACCOUNT_COOKIES).await.unwrap();

    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 10, max_age_ms: 3_600_000 }));
    let fp = Arc::new(Fingerprint::for_this_machine());
    let (_api, _client) = super::rebuild_api(&daemon_jar, &secrets, fp, live, TOKEN.into()).await.unwrap();

    assert_eq!(discord_cookies(&daemon_jar).as_deref(), Some("__dcfduid=new"));
    // Reloaded, not dirty: the saver won't write the old cookie back.
    assert!(!daemon_jar.is_dirty());
    assert!(!daemon_jar.save_if_dirty(&secrets, 100_000).await.unwrap());
    assert_eq!(secrets.get(crate::secrets::ACCOUNT_COOKIES).await.unwrap(), saved);
}

#[tokio::test]
async fn a_call_waiting_for_its_turn_does_not_go_out_after_a_halt() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_delay(Duration::from_millis(300))
                .set_body_json(json!({"captcha_key": ["captcha-required"]})),
        )
        .mount(&env.server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message_json(1, 101)))
        .mount(&env.server)
        .await;
    let msg = OutgoingMessage { content: "x".into(), reply_to: None, nonce: Id(1), silent: false };
    let (read, sent) = tokio::join!(env.api.history(Id(101), None, None, 5), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        env.api.send(Id(101), &msg).await
    });
    assert_eq!(read, Err(DiscordError::Captcha));
    assert_eq!(sent.map(|m| m.id), Err(DiscordError::Captcha));
    assert_eq!(received(&env).await.len(), 1, "the queued send never reached Discord");
}

#[tokio::test]
async fn a_halt_in_live_stops_rest_calls() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&env.server)
        .await;
    env.live.halt(DiscordError::Captcha);
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::Captcha));
    env.live.reject_token(TOKEN);
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::NeedsLogin));
    assert!(received(&env).await.is_empty());
}

#[tokio::test]
async fn a_401_asking_for_mfa_is_locked_and_keeps_the_token() {
    let env = env().await;
    Mock::given(method("DELETE"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "Two factor is required", "code": 60003})),
        )
        .mount(&env.server)
        .await;
    assert_eq!(env.api.leave_guild(Id(11)).await, Err(DiscordError::Locked));
    assert_eq!(env.live.connection(), ConnState::Halted(DiscordError::Locked));
    // The token wasn't marked dead.
    assert!(env.live.try_login_with(TOKEN));
}

#[tokio::test]
async fn clear_halt_lets_calls_through_again_after_a_captcha() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"captcha_key": ["captcha-required"]})))
        .up_to_n_times(1)
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&env.server)
        .await;
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::Captcha));
    // The user's explicit OK clears both halts (SendQueue::resume does this).
    env.live.clear_halt();
    env.api.clear_halt();
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Ok(vec![]));
    assert_eq!(received(&env).await.len(), 2, "the next call reached Discord");
}

#[tokio::test]
async fn clear_halt_never_lifts_a_dead_token() {
    let env = env().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&env.server).await;
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::NeedsLogin));
    env.live.clear_halt();
    env.api.clear_halt();
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::NeedsLogin));
    assert_eq!(received(&env).await.len(), 1, "a rejected token stays halted until a new login rebuilds the client");
}

#[tokio::test]
async fn clearing_a_block_keeps_calls_off_the_network_until_it_passes() {
    let env = env().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("content-type", "text/html")
                .insert_header("retry-after", "600")
                .set_body_string("<html>blocked</html>"),
        )
        .mount(&env.server)
        .await;
    assert_eq!(env.api.history(Id(101), None, None, 5).await, Err(DiscordError::Blocked { retry_after_s: 600 }));
    // The user says "continue" well inside the block window.
    env.live.clear_halt();
    env.api.clear_halt();
    match env.api.history(Id(101), None, None, 5).await {
        Err(DiscordError::RateLimited { retry_after_s }) => assert!(retry_after_s > 590, "{retry_after_s}"),
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert_eq!(received(&env).await.len(), 1, "nothing went out during the block");
}
