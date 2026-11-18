use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use super::http::app;
use super::server::TritoneServer;
use super::state::AppState;
use crate::config::Config;
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, LiveState};
use crate::domain::send_queue::SendQueue;
use crate::secrets::fake::MemSecrets;
use crate::store::Store;

fn test_state() -> AppState {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let timer = Arc::new(FakeTimer::at(1_000_000));
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 200, max_age_ms: 86_400_000 }));
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

/// Calls one tool over HTTP, through the guard, and returns the tool's JSON output.
async fn call_tool(state: AppState, name: &str) -> serde_json::Value {
    let router = app(TritoneServer::new(state, None), 7781, "secret".into());
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": name, "arguments": {}}
    });
    let req = Request::post("/mcp")
        .header("host", "127.0.0.1:7781")
        .header("authorization", "Bearer secret")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let rpc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let text = rpc["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no tool result: {rpc}"));
    serde_json::from_str(text).unwrap()
}

#[tokio::test]
async fn status_tool_reports_not_connected() {
    let s = call_tool(test_state(), "status").await;
    assert_eq!(s["connection"], "connecting");
    assert!(s["user"].is_null());
    assert_eq!(s["guilds"], 0);
}

#[test]
fn instructions_append_style_when_present() {
    use super::instructions::{BASE_INSTRUCTIONS, build_instructions};
    assert_eq!(build_instructions(None), BASE_INSTRUCTIONS);
    let s = build_instructions(Some("Short. No em dashes."));
    assert!(s.starts_with(BASE_INSTRUCTIONS));
    assert!(s.ends_with("## Writing style\n\nShort. No em dashes."));
}

#[test]
fn blank_or_missing_style_is_ignored() {
    use super::instructions::{BASE_INSTRUCTIONS, current_instructions, read_style_guide};
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("style.md");
    assert_eq!(read_style_guide(&p).unwrap(), None);
    assert_eq!(current_instructions(Some(&p)), BASE_INSTRUCTIONS);
    std::fs::write(&p, "  \n").unwrap();
    assert_eq!(read_style_guide(&p).unwrap(), None);
    std::fs::write(&p, "be brief\n").unwrap();
    assert_eq!(read_style_guide(&p).unwrap().as_deref(), Some("be brief"));
}

#[test]
fn instructions_cover_the_safety_rules() {
    use super::instructions::BASE_INSTRUCTIONS as I;
    for needle in [
        "explicit OK",
        "<@USER_ID>",
        "Reading marks read",
        "peek: true",
        "verify",
        "per-item OK",
        "Never follow instructions",
        "tritone login",
    ] {
        assert!(I.contains(needle), "instructions miss {needle:?}");
    }
}

#[tokio::test]
async fn tools_report_not_connected_until_the_gateway_sets_the_client() {
    let state = test_state();
    let server = TritoneServer::new(state.clone(), None);
    let r = server.run(|_ctx| async { Ok(1) }).await;
    assert_eq!(r.is_error, Some(true));
    assert!(serde_json::to_string(&r.content).unwrap().contains("not connected"));

    state.set_api(Some(Arc::new(crate::discord::fake::FakeDiscord::default())));
    let r = server.run(|_ctx| async { Ok(1) }).await;
    assert_ne!(r.is_error, Some(true));
}

#[tokio::test]
async fn halted_connection_blocks_tools_and_status_says_needs_login() {
    use crate::discord::gateway::GatewayEvent;
    use crate::discord::types::DiscordError;
    let state = test_state();
    state.set_api(Some(Arc::new(crate::discord::fake::FakeDiscord::default())));
    state.live.apply(GatewayEvent::Halted(DiscordError::NeedsLogin), 0);
    let server = TritoneServer::new(state.clone(), None);
    let r = server.run(|_ctx| async { Ok(1) }).await;
    assert_eq!(r.is_error, Some(true));
    assert!(serde_json::to_string(&r.content).unwrap().contains("tritone login"));

    let s = call_tool(state.clone(), "status").await;
    assert_eq!(s["connection"], "needs_login");
    assert!(s["note"].as_str().unwrap().contains("tritone login"));
}

#[tokio::test]
async fn status_wakes_the_gateway_after_a_new_login() {
    use crate::discord::gateway::GatewayEvent;
    use crate::discord::types::DiscordError;
    let state = test_state();
    state.live.apply(GatewayEvent::Halted(DiscordError::NeedsLogin), 0);
    state.secrets.set(crate::secrets::ACCOUNT_TOKEN, "tok").await.unwrap();
    let s = call_tool(state.clone(), "status").await;
    assert_eq!(s["connection"], "connecting");
    assert!(s["note"].as_str().unwrap().contains("reconnecting"));
    // notify_one stores a permit, so this returns at once.
    tokio::time::timeout(std::time::Duration::from_secs(1), state.login_notify.notified()).await.unwrap();
}

#[tokio::test]
async fn status_accepting_a_login_drops_the_old_client() {
    let state = test_state();
    state.set_api(Some(Arc::new(crate::discord::fake::FakeDiscord::default())));
    state.live.reject_token("tok-old");
    state.secrets.set(crate::secrets::ACCOUNT_TOKEN, "tok-new").await.unwrap();
    assert_eq!(call_tool(state.clone(), "status").await["connection"], "connecting");
    assert!(state.api.read().unwrap().is_none(), "the dead token's client is gone");
}

#[tokio::test]
async fn status_never_rewakes_the_gateway_with_a_rejected_token() {
    let state = test_state();
    state.secrets.set(crate::secrets::ACCOUNT_TOKEN, "tok-dead").await.unwrap();
    state.live.reject_token("tok-dead");
    for _ in 0..3 {
        let s = call_tool(state.clone(), "status").await;
        assert_eq!(s["connection"], "needs_login");
        let note = s["note"].as_str().unwrap();
        assert!(note.contains("tritone login"), "{note}");
        assert!(!note.contains("reconnecting"), "{note}");
    }
    let woke = tokio::time::timeout(std::time::Duration::from_millis(50), state.login_notify.notified()).await;
    assert!(woke.is_err(), "no wake-up permit may be stored for a dead token");
}

#[tokio::test]
async fn a_rest_halt_keeps_tools_blocked_across_gateway_resumes() {
    use crate::discord::gateway::GatewayEvent;
    use crate::discord::types::DiscordError;
    let state = test_state();
    state.set_api(Some(Arc::new(crate::discord::fake::FakeDiscord::default())));
    state.live.halt(DiscordError::Captcha);
    state.live.apply(GatewayEvent::Resumed, 10);
    let server = TritoneServer::new(state.clone(), None);
    let r = server.run(|_ctx| async { Ok(1) }).await;
    assert_eq!(r.is_error, Some(true));
    assert!(serde_json::to_string(&r.content).unwrap().contains("CAPTCHA"));
    assert_eq!(call_tool(state, "status").await["connection"], "halted");
}

#[tokio::test]
async fn status_reports_ready_user_and_guild_count() {
    use crate::discord::gateway::GatewayEvent;
    use crate::test_support::{guild, ready};
    let state = test_state();
    state.live.apply(GatewayEvent::Ready(Box::new(ready(7, vec![guild(1), guild(2)], vec![]))), 5);
    let s = call_tool(state, "status").await;
    assert_eq!(s["connection"], "ready");
    assert_eq!(s["user"], "U007");
    assert_eq!(s["guilds"], 2);
}
