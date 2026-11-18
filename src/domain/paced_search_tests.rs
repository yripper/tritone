use std::sync::Arc;

use super::*;
use crate::config::Config;
use crate::discord::DiscordApi;
use crate::discord::fake::Call;
use crate::discord::types::Id;
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::errors::Error;
use crate::domain::live::{BufferPolicy, LiveState};
use crate::domain::send_queue::SendQueue;
use crate::domain::test_api::{Api, T0};
use crate::store::Store;

fn rig() -> (Arc<Api>, Ctx) {
    let timer = Arc::new(FakeTimer::at(T0));
    let api = Arc::new(Api::new(timer.clone()));
    let store = Arc::new(Store::open_in_memory().unwrap());
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 86_400_000 }));
    let config = Arc::new(Config::default());
    let queue = Arc::new(SendQueue::new(
        store.clone(),
        live.clone(),
        config.clone(),
        timer.clone(),
        Arc::new(SeqGaps::new(&[0])),
    ));
    let ctx = Ctx { store, live, api: api.clone() as Arc<dyn DiscordApi>, queue, config, timer };
    (api, ctx)
}

fn is_search(c: &Call) -> bool {
    matches!(c, Call::Search { .. })
}

#[tokio::test]
async fn searches_are_spaced_by_a_random_gap() {
    let (api, ctx) = rig();
    let pacer = SearchPacer::new(Arc::new(SeqGaps::new(&[4_000, 1, 99_000])));
    let q = SearchQuery::default();
    for _ in 0..3 {
        pacer.search(&ctx, SearchScope::Guild(Id(10)), &q).await.unwrap();
    }
    // The first goes at once; the gaps are clamped to 3–6 s.
    assert_eq!(api.times_of(is_search), [0, 4_000, 7_000]);
}

#[tokio::test]
async fn indexing_is_waited_out_then_retried() {
    let (api, ctx) = rig();
    api.fail_on("search", DiscordError::Indexing { retry_after_s: 0 });
    let pacer = SearchPacer::new(Arc::new(SeqGaps::new(&[3_000])));
    pacer.search(&ctx, SearchScope::Guild(Id(10)), &SearchQuery::default()).await.unwrap();
    // 0 s means the default 5 s; the gap after it has passed by then.
    assert_eq!(api.times_of(is_search), [0, 5_000]);
}

#[tokio::test]
async fn halting_error_halts_and_later_searches_refuse() {
    let (api, ctx) = rig();
    api.fail_on("search", DiscordError::Captcha);
    let pacer = SearchPacer::new(Arc::new(SeqGaps::new(&[3_000])));
    let q = SearchQuery::default();
    let e = pacer.search(&ctx, SearchScope::Guild(Id(10)), &q).await.unwrap_err();
    assert!(matches!(e, Error::Discord(DiscordError::Captcha)));
    assert_eq!(ctx.live.connection(), ConnState::Halted(DiscordError::Captcha));
    assert!(pacer.search(&ctx, SearchScope::Guild(Id(10)), &q).await.is_err());
    assert_eq!(api.times_of(is_search).len(), 1, "no request while halted");
}
