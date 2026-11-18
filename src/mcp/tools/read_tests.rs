//! The read and triage tools over HTTP, through the guard, against
//! `FakeDiscord` and a READY fixture.

use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::config::Config;
use crate::discord::fake::{Call, FakeDiscord};
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{Channel, Id, Message, SearchQuery, SearchScope};
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, LiveState};
use crate::domain::send_queue::SendQueue;
use crate::mcp::http::app;
use crate::mcp::server::TritoneServer;
use crate::mcp::state::AppState;
use crate::secrets::fake::MemSecrets;
use crate::store::{Store, WatchLevel};
use crate::test_support::*;

const ME: u64 = 7;
const NOW: i64 = 1_800_000_000_000;

fn at(age_ms: i64) -> Id {
    id_at(NOW - age_ms)
}

fn named(id: u64, guild: u64, name: &str) -> Channel {
    Channel { name: Some(name.into()), ..text_channel(id, guild) }
}

/// G001 and G002, each with #general (101, 111); G001 also #help (102). An
/// unread DM D301 with U002.
fn state() -> (AppState, Arc<FakeDiscord>) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let timer = Arc::new(FakeTimer::at(NOW));
    let queue = Arc::new(SendQueue::new(store.clone(), timer.clone(), Arc::new(SeqGaps::new(&[0]))));
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 86_400_000 }));
    let mut d = dm(301, 2);
    d.last_message_id = Some(at(1_000));
    let mut help = named(102, 1, "help");
    help.last_message_id = Some(at(1_000));
    let mut snap =
        ready(ME, vec![guild(1), guild(2)], vec![named(101, 1, "general"), help, named(111, 2, "general"), d]);
    snap.read_states.insert(Id(301), read_state(Some(at(5_000)), 1));
    snap.read_states.insert(Id(102), read_state(Some(at(5_000)), 0));
    live.apply(GatewayEvent::Ready(Box::new(snap)), NOW);
    let state = AppState::new(store, Arc::new(MemSecrets::default()), live, queue, Arc::new(Config::default()), timer);
    let fake = Arc::new(FakeDiscord::new(user(ME)));
    state.set_api(Some(fake.clone()));
    (state, fake)
}

async fn rpc(state: &AppState, method: &str, params: Value) -> Value {
    let router = app(TritoneServer::new(state.clone(), None), 7781, "secret".into());
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let req = Request::post("/mcp")
        .header("host", "127.0.0.1:7781")
        .header("authorization", "Bearer secret")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// `Ok(json)` for a tool result, `Err(text)` for a tool error.
async fn call(state: &AppState, name: &str, args: Value) -> Result<Value, String> {
    let rpc = rpc(state, "tools/call", json!({"name": name, "arguments": args})).await;
    let result = &rpc["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no tool result: {rpc}")).to_string();
    if result["isError"] == true { Err(text) } else { Ok(serde_json::from_str(&text).unwrap()) }
}

fn acks(fake: &FakeDiscord) -> usize {
    fake.calls().iter().filter(|c| matches!(c, Call::Ack { .. } | Call::AckBulk(_))).count()
}

#[tokio::test]
async fn read_and_triage_tools_are_listed() {
    let (state, _) = state();
    let list = rpc(&state, "tools/list", json!({})).await;
    let names: Vec<&str> =
        list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for n in [
        "list_servers",
        "list_channels",
        "get_pending",
        "read_history",
        "read_thread",
        "search_messages",
        "get_channel_info",
        "find_user",
        "mark_read",
        "snooze",
        "unsnooze",
        "set_watch_level",
    ] {
        assert!(names.contains(&n), "missing {n}: {names:?}");
    }
}

#[tokio::test]
async fn get_pending_tool_lists_without_acking() {
    let (state, fake) = state();
    fake.put_history(Id(301), vec![msg(at(1_000), 301, 2, "x")]);
    let v = call(&state, "get_pending", json!({})).await.unwrap();
    let items = v["pending"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{v}");
    assert_eq!(items[0]["reason"], "dm");
    assert_eq!(items[0]["label"], "@U002 (DM)");
    assert_eq!(items[0]["channel"], "301");
    assert_eq!(items[0]["messages"][0]["from"], "U002");
    assert_eq!(items[0]["more_unread"], false);
    assert_eq!(acks(&fake), 0);
}

#[tokio::test]
async fn read_history_by_name_marks_read_and_ambiguity_is_an_error() {
    let (state, fake) = state();
    let err = call(&state, "read_history", json!({"channel": "#general"})).await.unwrap_err();
    assert!(err.contains("matches several") && err.contains("G001 › #general (101)"), "{err}");
    assert!(fake.calls().is_empty(), "nothing read on an ambiguous name");

    let m = Message { guild_id: Some(Id(1)), ..msg(at(1_000), 102, 2, "x") };
    fake.put_history(Id(102), vec![m]);
    let peek = call(&state, "read_history", json!({"channel": "#help", "peek": true})).await.unwrap();
    assert_eq!(peek["messages"].as_array().unwrap().len(), 1);
    assert_eq!(acks(&fake), 0);
    let v = call(&state, "read_history", json!({"channel": "G001/#help"})).await.unwrap();
    assert_eq!(v["newest_id"], at(1_000).to_string());
    assert!(fake.calls().contains(&Call::History { channel: Id(102), before: None, after: None, limit: 50 }));
    assert_eq!(acks(&fake), 1);
}

#[tokio::test]
async fn read_thread_uses_the_thread_id() {
    let (state, fake) = state();
    let r = call(&state, "read_thread", json!({"thread": "4242", "limit": 5, "peek": true})).await.unwrap();
    assert_eq!(r["messages"].as_array().unwrap().len(), 0);
    assert!(fake.calls().contains(&Call::History { channel: Id(4242), before: None, after: None, limit: 5 }));
}

#[tokio::test]
async fn search_needs_a_scope_and_picks_the_right_endpoint() {
    let (state, fake) = state();
    let err = call(&state, "search_messages", json!({"query": "rust"})).await.unwrap_err();
    assert!(err.contains("server or a channel"), "{err}");
    call(&state, "search_messages", json!({"query": "rust", "channel": "G001/#help", "author": "U002"})).await.unwrap();
    call(&state, "search_messages", json!({"query": "rust", "channel": "@U002"})).await.unwrap();
    call(&state, "search_messages", json!({"query": "rust", "server": "G002"})).await.unwrap();
    let searches: Vec<(SearchScope, SearchQuery)> = fake
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Search { scope, query } => Some((scope, query)),
            _ => None,
        })
        .collect();
    let content = Some("rust".to_string());
    assert_eq!(
        searches,
        vec![
            (
                SearchScope::Guild(Id(1)),
                SearchQuery {
                    content: content.clone(),
                    author_id: Some(Id(2)),
                    channel_id: Some(Id(102)),
                    ..Default::default()
                }
            ),
            (SearchScope::Channel(Id(301)), SearchQuery { content: content.clone(), ..Default::default() }),
            (SearchScope::Guild(Id(2)), SearchQuery { content, ..Default::default() }),
        ]
    );
}

#[tokio::test]
async fn listings_info_and_find_user() {
    let (state, _) = state();
    let servers = call(&state, "list_servers", json!({})).await.unwrap();
    assert_eq!(servers["servers"].as_array().unwrap().len(), 2);
    let chans = call(&state, "list_channels", json!({"server": "G001"})).await.unwrap();
    let labels: Vec<&str> =
        chans["channels"].as_array().unwrap().iter().map(|c| c["label"].as_str().unwrap()).collect();
    assert_eq!(labels, vec!["G001 › #general", "G001 › #help"]);
    assert_eq!(chans["truncated"], false);
    let dms = call(&state, "list_channels", json!({"kind": "dm"})).await.unwrap();
    assert_eq!(dms["channels"].as_array().unwrap().len(), 1);
    let info = call(&state, "get_channel_info", json!({"channel": "@U002"})).await.unwrap();
    assert_eq!(
        (info["kind"].as_str(), info["watch"].as_str(), info["unread"].as_bool()),
        (Some("dm"), Some("full"), Some(true))
    );
    let users = call(&state, "find_user", json!({"query": "u002"})).await.unwrap();
    assert_eq!(users["users"][0]["id"], "2");
    assert_eq!(users["users"][0]["dm_channel"], "301");
}

#[tokio::test]
async fn triage_tools() {
    let (state, fake) = state();
    let r = call(&state, "mark_read", json!({"channels": ["@U002", "G001/#help", "101"]})).await.unwrap();
    assert_eq!(r["marked"], json!(["301", "102"]));
    assert_eq!(r["already_read"], json!(["101"]));
    assert_eq!(fake.calls(), vec![Call::AckBulk(vec![(Id(301), at(1_000)), (Id(102), at(1_000))])]);
    let err = call(&state, "mark_read", json!({"channels": ["#general"]})).await.unwrap_err();
    assert!(err.contains("matches several"), "{err}");
    assert_eq!(fake.calls().len(), 1, "an ambiguous name acks nothing");

    let err = call(&state, "snooze", json!({"channel": "#help", "until": "2020-01-01T00:00:00Z"})).await.unwrap_err();
    assert!(err.contains("past"), "{err}");
    call(&state, "snooze", json!({"channel": "#help", "until": "2027-02-01T09:00:00-03:00"})).await.unwrap();
    assert_eq!(state.store.snoozed_until(Id(102)).unwrap(), Some(1_801_483_200_000));
    assert!(
        call(&state, "snooze", json!({"channel": "#help", "until": "tomorrow"}))
            .await
            .unwrap_err()
            .contains("RFC 3339")
    );
    call(&state, "unsnooze", json!({"channel": "#help"})).await.unwrap();
    assert_eq!(state.store.snoozed_until(Id(102)).unwrap(), None);

    let v = call(&state, "set_watch_level", json!({"target": "G002", "level": "full"})).await.unwrap();
    assert_eq!(v["kind"], "server");
    assert_eq!(state.store.get_watch(Id(2)).unwrap(), Some(WatchLevel::Full));
    let v = call(&state, "set_watch_level", json!({"target": "G001/#help", "level": "muted"})).await.unwrap();
    assert_eq!(v["kind"], "channel");
    // A muted channel still resolves, so it can be unmuted by name.
    call(&state, "set_watch_level", json!({"target": "#help", "level": "clear"})).await.unwrap();
    assert_eq!(state.store.get_watch(Id(102)).unwrap(), None);
}
