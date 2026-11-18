use std::sync::Arc;

use super::*;
use crate::config::Config;
use crate::discord::DiscordApi;
use crate::discord::fake::FakeDiscord;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::Id;
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, LiveState};
use crate::domain::send_queue::SendQueue;
use crate::mcp::state::AppState;
use crate::secrets::fake::MemSecrets;
use crate::store::{ActivityRow, Store};
use crate::test_support::{guild, id_at, msg, ready, text_channel};

const T0: i64 = 1_800_000_000_000;

fn server(ready_now: bool) -> (TritoneServer, Arc<FakeDiscord>) {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let timer = Arc::new(FakeTimer::at(T0));
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 86_400_000 }));
    if ready_now {
        live.apply(GatewayEvent::Ready(Box::new(ready(1, vec![guild(10)], vec![text_channel(100, 10)]))), T0);
    }
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

fn out(r: &CallToolResult) -> serde_json::Value {
    let v = serde_json::to_value(r).unwrap();
    let text = v["content"][0]["text"].as_str().unwrap_or_default().to_string();
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

#[tokio::test]
async fn my_activity_counts_the_period() {
    let (s, _) = server(true);
    let row = |at: i64| ActivityRow {
        message_id: id_at(at),
        guild_id: Id(10),
        channel_id: Id(100),
        created_at: at,
        kind: "answer".into(),
        is_reply: true,
        via_tritone: false,
    };
    s.state.store.record_activity(&row(T0 - DAY_MS)).unwrap();
    s.state.store.record_activity(&row(T0 - 40 * DAY_MS)).unwrap();
    let v = out(&s.my_activity(Parameters(MyActivityArgs { days: None })).await.unwrap());
    assert_eq!(v["total"], 1, "30 days by default");
    let v = out(&s.my_activity(Parameters(MyActivityArgs { days: Some(60) })).await.unwrap());
    assert_eq!(v["total"], 2);
}

#[tokio::test]
async fn backfill_activity_refuses_before_ready_then_runs_in_the_background() {
    let (s, api) = server(false);
    let r = s.backfill_activity().await.unwrap();
    assert_eq!(r.is_error, Some(true), "no guilds known yet");

    let (s, api2) = server(true);
    drop(api);
    let mut m = msg(id_at(T0), 100, 1, "x");
    m.guild_id = Some(Id(10));
    api2.search_index.lock().unwrap().push(m);
    let v = out(&s.backfill_activity().await.unwrap());
    assert_eq!(v["running"], true, "{v}");
    for _ in 0..200 {
        if !BACKFILL_RUNNING.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    let v = out(&s.backfill_activity().await.unwrap());
    assert_eq!((v["running"].clone(), v["guilds_done"].clone()), (false.into(), 1.into()), "{v}");
    assert!(v["note"].as_str().unwrap().contains("complete"));
    assert_eq!(s.state.store.activity_between(0, i64::MAX).unwrap().len(), 1);
}
