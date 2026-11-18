use std::sync::Arc;

use super::{Notified, forget_login, notify_daemon, render_qr, save_login};
use crate::config::Config;
use crate::discord::transport::CookieJar;
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, ConnState, LiveState};
use crate::domain::send_queue::SendQueue;
use crate::mcp::server::TritoneServer;
use crate::mcp::state::AppState;
use crate::secrets::fake::MemSecrets;
use crate::secrets::{ACCOUNT_BEARER, ACCOUNT_COOKIES, ACCOUNT_TOKEN, SecretStore};
use crate::store::Store;

#[test]
fn render_qr_draws_the_url_in_blocks() {
    let qr = render_qr("https://discord.com/ra/abc").unwrap();
    assert!(qr.lines().count() > 10);
    assert!(qr.contains('█') || qr.contains('▀') || qr.contains('▄'));
}

#[tokio::test]
async fn save_and_forget_a_login() {
    let s = MemSecrets::default();
    s.set(ACCOUNT_BEARER, "b").await.unwrap();
    let jar = CookieJar::empty();
    save_login(&s, &jar, "fake-token-0001").await.unwrap();
    assert_eq!(s.get(ACCOUNT_TOKEN).await.unwrap().as_deref(), Some("fake-token-0001"));
    s.set(ACCOUNT_COOKIES, "x").await.unwrap();
    forget_login(&s).await.unwrap();
    assert_eq!(s.get(ACCOUNT_TOKEN).await.unwrap(), None);
    assert_eq!(s.get(ACCOUNT_COOKIES).await.unwrap(), None);
    // The MCP bearer stays: logging out of Discord doesn't unregister Claude.
    assert_eq!(s.get(ACCOUNT_BEARER).await.unwrap().as_deref(), Some("b"));
}

fn daemon_state() -> AppState {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let timer = Arc::new(FakeTimer::at(1_000_000));
    let queue = Arc::new(SendQueue::new(store.clone(), timer.clone(), Arc::new(SeqGaps::new(&[0]))));
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 10, max_age_ms: 3_600_000 }));
    AppState::new(store, Arc::new(MemSecrets::default()), live, queue, Arc::new(Config::default()), timer)
}

#[tokio::test]
async fn notify_reaches_a_running_daemon() {
    let state = daemon_state();
    state.live.reject_token("fake-token-old");
    state.secrets.set(ACCOUNT_TOKEN, "fake-token-new").await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = crate::mcp::http::app(TritoneServer::new(state.clone(), None), port, "secret".into());
    let server = tokio::spawn(async move { axum::serve(listener, router).await });

    let got = notify_daemon(port, "secret").await.unwrap();
    server.abort();
    match got {
        Notified::Answered { status, text } => {
            assert_eq!(status, 200, "{text}");
            assert!(text.contains("reconnecting"), "{text}");
        }
        Notified::NotRunning => panic!("the daemon is running"),
    }
    assert_eq!(state.live.connection(), ConnState::Connecting);
}

#[tokio::test]
async fn notify_without_a_daemon_says_not_running() {
    // Bind then drop: the port is very likely free and closed.
    let port = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    assert!(matches!(notify_daemon(port, "secret").await.unwrap(), Notified::NotRunning));
}
