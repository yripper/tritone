use std::sync::Arc;

use super::*;
use crate::config::Config;
use crate::discord::DiscordApi;
use crate::discord::fake::FakeDiscord;
use crate::discord::gateway::GatewayEvent;
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, LiveState};
use crate::domain::send_queue::SendQueue;
use crate::mcp::state::AppState;
use crate::secrets::fake::MemSecrets;
use crate::store::Store;
use crate::test_support::{guild, id_at, msg, ready, text_channel};

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
        Arc::new(SeqGaps::new(&[0])),
    ));
    let state = AppState::new(store, Arc::new(MemSecrets::default()), live, queue, config, timer);
    let api = Arc::new(FakeDiscord::default());
    state.set_api(Some(api.clone() as Arc<dyn DiscordApi>));
    (TritoneServer::new(state, None), api)
}

fn out(r: &CallToolResult) -> serde_json::Value {
    let v = serde_json::to_value(r).unwrap();
    let text = v["content"][0]["text"].as_str().unwrap_or_default().to_string();
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

fn ch(c: &str) -> Parameters<ChannelArgs> {
    Parameters(ChannelArgs { channel: c.into() })
}

#[tokio::test]
async fn voice_tools_save_and_read_back() {
    let (s, api) = server();
    let mut m = msg(id_at(T0 - 1_000), 100, 1, "fake text");
    m.guild_id = Some(Id(10));
    api.search_index.lock().unwrap().push(m);

    let v = out(&s.get_voice(ch("https://discord.com/channels/10/100")).await.unwrap());
    assert_eq!((v["category"].clone(), v["needs_learning"].clone()), ("general".into(), true.into()), "{v}");
    assert_eq!(v["my_messages_in_channel"][0]["text"], "fake text");

    let set = |t: &str, c: &str| Parameters(CategoryArgs { target: t.into(), category: c.into() });
    assert_eq!(out(&s.set_category(set("10", "dev-community")).await.unwrap())["ok"], true, "a server id");
    let r = s.set_category(set("10", "Dev Community")).await.unwrap();
    assert_eq!(r.is_error, Some(true), "bad slug");
    let profile = Parameters(ProfileArgs { category: "dev-community".into(), body: "short".into() });
    assert_eq!(out(&s.save_profile(profile).await.unwrap())["ok"], true);
    let notes = Parameters(NotesArgs { channel: "100".into(), body: "links docs".into() });
    assert_eq!(out(&s.save_chat_notes(notes).await.unwrap())["ok"], true);

    let calls = api.calls().len();
    let v = out(&s.get_voice(ch("<#100>")).await.unwrap());
    assert_eq!(v["category"], "dev-community", "from the server");
    assert_eq!((v["profile"].clone(), v["notes"].clone()), ("short".into(), "links docs".into()));
    assert!(v.get("my_messages_in_channel").is_none(), "fresh: nothing fetched");
    assert_eq!(api.calls().len(), calls);
    assert_eq!(out(&s.list_stale_profiles().await.unwrap())["categories"], serde_json::json!([]));
}

#[tokio::test]
async fn unknown_channels_are_refused() {
    let (s, _) = server();
    assert_eq!(s.get_voice(ch("999")).await.unwrap().is_error, Some(true));
    let notes = Parameters(NotesArgs { channel: "general".into(), body: "x".into() });
    assert_eq!(s.save_chat_notes(notes).await.unwrap().is_error, Some(true));
}

#[tokio::test]
async fn cleanup_tools_plan_then_apply_only_the_given_items() {
    let (s, api) = server();
    let v = out(&s.cleanup_plan().await.unwrap());
    let g = &v["servers"][0];
    assert_eq!((g["id"].clone(), g["suggestion"].clone()), ("10".into(), "leave".into()), "{v}");
    assert_eq!(g["items"][0]["item"], "leave:10");
    assert!(
        !api.calls().iter().any(|c| matches!(c, crate::discord::fake::Call::LeaveGuild(_))),
        "a plan changes nothing"
    );

    let args = Parameters(ApplyArgs { items: vec!["mute:10".into()] });
    let v = out(&s.cleanup_apply(args).await.unwrap());
    assert_eq!(v["results"][0]["status"], "done", "{v}");
    assert!(!api.calls().iter().any(|c| matches!(c, crate::discord::fake::Call::LeaveGuild(_))));
}
