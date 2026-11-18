use std::sync::Arc;

use super::*;
use crate::config::Config;
use crate::discord::DiscordApi;
use crate::discord::fake::FakeDiscord;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::DiscordError;
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, ConnState};
use crate::domain::send_queue::SendQueue;
use crate::mcp::state::AppState;
use crate::secrets::fake::MemSecrets;
use crate::store::Store;
use crate::test_support::{guild, ready, text_channel};

const T0: i64 = 1_800_000_000_000;

fn server() -> (TritoneServer, Arc<FakeDiscord>) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let timer = Arc::new(FakeTimer::at(T0));
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 86_400_000 }));
    live.apply(GatewayEvent::Ready(Box::new(ready(1, vec![guild(10)], vec![text_channel(100, 10)]))), T0);
    let config = Arc::new(Config::default());
    let queue = Arc::new(SendQueue::new(
        store.clone(),
        live.clone(),
        config.clone(),
        timer.clone(),
        Arc::new(SeqGaps::new(&[20_000])),
    ));
    let state = AppState::new(store, Arc::new(MemSecrets::default()), live, queue, config, timer);
    let api = Arc::new(FakeDiscord::default());
    state.set_api(Some(api.clone() as Arc<dyn DiscordApi>));
    (TritoneServer::new(state, None), api)
}

fn text(r: &CallToolResult) -> String {
    let v = serde_json::to_value(r).unwrap();
    v["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

fn send_args(channel: &str) -> SendArgs {
    SendArgs { channel: channel.into(), text: "hello".into(), reply_to: None, kind: None }
}

#[tokio::test]
async fn send_message_queues_without_sending() {
    let (s, api) = server();
    let r = s.send_message(Parameters(send_args("100"))).await.unwrap();
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    let v: serde_json::Value = serde_json::from_str(&text(&r)).unwrap();
    assert_eq!(v["job_id"], 1);
    assert!(api.calls().is_empty(), "nothing goes out inside the call");
    let st = s.state.queue.status(Some(1)).unwrap();
    assert_eq!(st.jobs[0].target, "G010 › #C100");
}

#[tokio::test]
async fn send_message_takes_a_link_and_refuses_unknown_channels() {
    let (s, _) = server();
    let r = s.send_message(Parameters(send_args("https://discord.com/channels/10/100/555"))).await.unwrap();
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    let r = s.send_message(Parameters(send_args("999"))).await.unwrap();
    assert_eq!(r.is_error, Some(true));
    let r = s.send_message(Parameters(send_args("general"))).await.unwrap();
    assert!(text(&r).contains("channel's id"), "{}", text(&r));
}

#[tokio::test]
async fn forum_post_needs_a_forum() {
    let (s, _) = server();
    let r = s
        .create_forum_post(Parameters(ForumPostArgs { forum: "100".into(), title: "t".into(), text: "x".into() }))
        .await
        .unwrap();
    assert!(text(&r).contains("not a forum"));
}

#[tokio::test]
async fn resume_and_status_work_while_halted() {
    let (s, _) = server();
    s.state.live.halt(DiscordError::Captcha);
    let r = s.send_message(Parameters(send_args("100"))).await.unwrap();
    assert_eq!(r.is_error, Some(true), "writes refuse while halted");
    let r = s.send_status(Parameters(SendStatusArgs { job_id: None })).await.unwrap();
    assert_ne!(r.is_error, Some(true));
    let r = s.resume_queue().await.unwrap();
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    assert!(matches!(s.state.live.connection(), ConnState::Ready { .. }));
}

#[tokio::test]
async fn resume_refuses_needs_login() {
    let (s, _) = server();
    s.state.live.reject_token("dead");
    let r = s.resume_queue().await.unwrap();
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).contains("tritone login"));
}

#[tokio::test]
async fn resume_queue_lifts_the_rest_clients_own_halt() {
    let (s, _) = server();
    let timer = Arc::new(FakeTimer::at(T0));
    let api = Arc::new(crate::domain::test_api::Api::new(timer));
    s.state.set_api(Some(api.clone() as Arc<dyn DiscordApi>));
    s.state.live.halt(DiscordError::Captcha);
    let r = s.resume_queue().await.unwrap();
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    assert_eq!(api.cleared(), 1, "the client AppState holds now, not a stale one");
}
