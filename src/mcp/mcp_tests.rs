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
    let queue = Arc::new(SendQueue::new(store.clone(), timer.clone(), Arc::new(SeqGaps::new(&[0]))));
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 200, max_age_ms: 86_400_000 }));
    AppState::new(store, Arc::new(MemSecrets::default()), live, queue, Arc::new(Config::default()), timer)
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

struct NoopApi;
impl crate::discord::DiscordApi for NoopApi {}

#[tokio::test]
async fn tools_report_not_connected_until_the_gateway_sets_the_client() {
    let state = test_state();
    let server = TritoneServer::new(state.clone(), None);
    let r = server.run(|_ctx| async { Ok(1) }).await;
    assert_eq!(r.is_error, Some(true));
    assert!(serde_json::to_string(&r.content).unwrap().contains("not connected"));

    state.set_api(Some(Arc::new(NoopApi)));
    let r = server.run(|_ctx| async { Ok(1) }).await;
    assert_ne!(r.is_error, Some(true));
}
