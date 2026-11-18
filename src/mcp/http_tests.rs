use super::guard_router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn guard_rejects_foreign_host_and_bad_tokens() {
    let app =
        guard_router(axum::Router::new().route("/mcp", axum::routing::post(|| async { "ok" })), 7781, "secret".into());
    let req = |host: &str, auth: Option<&str>| {
        let mut b = Request::post("/mcp").header("host", host);
        if let Some(a) = auth {
            b = b.header("authorization", a);
        }
        b.body(Body::empty()).unwrap()
    };
    let status = |r: axum::response::Response| r.status();
    assert_eq!(
        status(app.clone().oneshot(req("evil.com:7781", Some("Bearer secret"))).await.unwrap()),
        StatusCode::FORBIDDEN
    );
    assert_eq!(status(app.clone().oneshot(req("127.0.0.1:7781", None)).await.unwrap()), StatusCode::UNAUTHORIZED);
    assert_eq!(
        status(app.clone().oneshot(req("127.0.0.1:7781", Some("Bearer nope"))).await.unwrap()),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(app.clone().oneshot(req("localhost:7781", Some("Bearer secret"))).await.unwrap()),
        StatusCode::OK
    );
    assert_eq!(status(app.oneshot(req("127.0.0.1:7781", Some("Bearer secret"))).await.unwrap()), StatusCode::OK);
}

mod login_notify {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::discord::types::DiscordError;
    use crate::domain::clock::fake::{FakeTimer, SeqGaps};
    use crate::domain::live::{BufferPolicy, ConnState, LiveState};
    use crate::domain::send_queue::SendQueue;
    use crate::mcp::http::app;
    use crate::mcp::server::TritoneServer;
    use crate::mcp::state::AppState;
    use crate::secrets::ACCOUNT_TOKEN;
    use crate::secrets::fake::MemSecrets;
    use crate::store::Store;

    fn state() -> AppState {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let timer = Arc::new(FakeTimer::at(1_000_000));
        let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 10, max_age_ms: 3_600_000 }));
        let config = Arc::new(Config::default());
        let queue = Arc::new(SendQueue::new(
            store.clone(),
            live.clone(),
            config.clone(),
            timer.clone(),
            Arc::new(SeqGaps::new(&[0])),
        ));
        AppState::new(store, Arc::new(MemSecrets::default()), live, queue, config, timer)
    }

    async fn post(state: &AppState, auth: &str) -> (StatusCode, String) {
        let router = app(TritoneServer::new(state.clone(), None), 7781, "secret".into());
        let req = Request::post("/login-notify")
            .header("host", "127.0.0.1:7781")
            .header("authorization", auth)
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn notified(state: &AppState) -> bool {
        tokio::time::timeout(Duration::from_millis(100), state.login_notify.notified()).await.is_ok()
    }

    #[tokio::test]
    async fn a_new_login_wakes_the_daemon() {
        let s = state();
        s.live.reject_token("fake-token-old");
        s.secrets.set(ACCOUNT_TOKEN, "fake-token-new").await.unwrap();
        let (status, text) = post(&s, "Bearer secret").await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(s.live.connection(), ConnState::Connecting);
        assert!(notified(&s).await, "the gateway is woken");
    }

    #[tokio::test]
    async fn the_rejected_login_does_not_wake_it() {
        let s = state();
        s.live.reject_token("fake-token-old");
        s.secrets.set(ACCOUNT_TOKEN, "fake-token-old").await.unwrap();
        let (status, text) = post(&s, "Bearer secret").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(text.contains("tritone login"), "{text}");
        assert_eq!(s.live.connection(), ConnState::Halted(DiscordError::NeedsLogin));
        assert!(!notified(&s).await);
    }

    #[tokio::test]
    async fn no_login_in_the_keyring_is_a_conflict() {
        let s = state();
        let (status, _) = post(&s, "Bearer secret").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(!notified(&s).await);
    }

    #[tokio::test]
    async fn a_login_does_not_lift_another_halt() {
        let s = state();
        s.live.halt(DiscordError::Captcha);
        s.secrets.set(ACCOUNT_TOKEN, "fake-token-new").await.unwrap();
        let (status, text) = post(&s, "Bearer secret").await;
        assert_eq!(status, StatusCode::CONFLICT, "{text}");
        assert!(text.contains("restart"), "{text}");
        assert_eq!(s.live.connection(), ConnState::Halted(DiscordError::Captcha));
        assert!(!notified(&s).await);
    }

    #[tokio::test]
    async fn a_login_while_connected_does_not_reconnect() {
        let s = state();
        s.secrets.set(ACCOUNT_TOKEN, "fake-token-new").await.unwrap();
        let before = s.live.connection();
        let (status, text) = post(&s, "Bearer secret").await;
        assert_eq!(status, StatusCode::CONFLICT, "{text}");
        assert_eq!(s.live.connection(), before);
        assert!(!notified(&s).await);
    }

    #[tokio::test]
    async fn an_accepted_login_drops_the_old_client() {
        let s = state();
        s.set_api(Some(Arc::new(crate::discord::fake::FakeDiscord::default())));
        s.live.reject_token("fake-token-old");
        s.secrets.set(ACCOUNT_TOKEN, "fake-token-new").await.unwrap();
        assert_eq!(post(&s, "Bearer secret").await.0, StatusCode::OK);
        // Until the gateway builds the new client, nothing can call Discord with the dead token.
        assert!(s.api.read().unwrap().is_none());
    }

    #[tokio::test]
    async fn it_sits_behind_the_guard() {
        let s = state();
        s.secrets.set(ACCOUNT_TOKEN, "fake-token-new").await.unwrap();
        assert_eq!(post(&s, "Bearer nope").await.0, StatusCode::UNAUTHORIZED);
        assert!(!notified(&s).await);
    }
}
