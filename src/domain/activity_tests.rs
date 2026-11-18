use std::sync::Arc;

use super::*;
use crate::config::{Config, Topic};
use crate::discord::DiscordApi;
use crate::discord::fake::Call;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{DiscordError, Id, Message, SearchQuery, SearchScope};
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, ConnState};
use crate::domain::paced_search::SearchPacer;
use crate::domain::send_queue::SendQueue;
use crate::domain::test_api::{Api, T0};
use crate::mcp::state::Ctx;
use crate::test_support::{dm, guild, id_at, msg, ready, text_channel};

const ME: u64 = 1;

fn topics() -> Vec<Topic> {
    vec![Topic { kind: "rust".into(), keywords: vec!["rust".into(), "cargo".into()] }]
}

fn live() -> Arc<LiveState> {
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 86_400_000 }));
    let mut help = text_channel(101, 10);
    help.name = Some("cargo-help".into());
    let channels = vec![text_channel(100, 10), help, text_channel(200, 20), dm(300, 2)];
    live.apply(GatewayEvent::Ready(Box::new(ready(ME, vec![guild(10), guild(20)], channels))), T0);
    live.set_buffered([Id(100), Id(101)].into_iter().collect());
    live
}

/// A message in a guild channel at `T0 + n` s.
fn gmsg(n: i64, channel: u64, guild: u64, author: u64, text: &str) -> Message {
    let mut m = msg(id_at(T0 + n * 1_000), channel, author, text);
    m.guild_id = Some(Id(guild));
    m
}

fn reply(mut m: Message, to: &Message) -> Message {
    m.reply_to = Some(to.id);
    m.reply_to_author = Some(to.author.id);
    m.kind = 19;
    m
}

/// A search pacer with a fixed gap.
fn pacer(gap: u64) -> SearchPacer {
    SearchPacer::new(Arc::new(SeqGaps::new(&[gap])))
}

struct Rig {
    api: Arc<Api>,
    ctx: Ctx,
}

fn rig() -> Rig {
    let timer = Arc::new(FakeTimer::at(T0));
    let api = Arc::new(Api::new(timer.clone()));
    let store = Arc::new(Store::open_in_memory().unwrap());
    let live = live();
    let config = Arc::new(Config { topics: topics(), ..Config::default() });
    let queue = Arc::new(SendQueue::new(
        store.clone(),
        live.clone(),
        config.clone(),
        timer.clone(),
        Arc::new(SeqGaps::new(&[20_000])),
    ));
    let ctx = Ctx { store, live, api: api.clone() as Arc<dyn DiscordApi>, queue, config, timer };
    Rig { api, ctx }
}

fn index(r: &Rig, msgs: Vec<Message>) {
    r.api.inner.search_index.lock().unwrap().extend(msgs);
}

fn searches(api: &Api) -> Vec<(SearchScope, u32)> {
    api.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Search { scope, query } => Some((scope, query.offset)),
            _ => None,
        })
        .collect()
}

fn is_search(c: &Call) -> bool {
    matches!(c, Call::Search { .. })
}

async fn until_rows(store: &Store, n: usize) {
    for _ in 0..200 {
        if store.activity_between(0, i64::MAX).unwrap().len() >= n {
            return;
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn own_guild_message_recorded() {
    let live = live();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let config = Arc::new(Config { topics: topics(), ..Config::default() });
    let task = spawn_recorder(live.clone(), store.clone(), config);
    let q = gmsg(1, 100, 10, 2, "how do I do this?");
    live.apply(GatewayEvent::MessageCreate(Box::new(q.clone())), T0);
    let mine = reply(gmsg(2, 100, 10, ME, "like this"), &q);
    live.apply(GatewayEvent::MessageCreate(Box::new(mine.clone())), T0);
    until_rows(&store, 1).await;
    let rows = store.activity_between(0, i64::MAX).unwrap();
    assert_eq!(rows.len(), 1, "only the user's own message");
    let r = &rows[0];
    assert_eq!((r.message_id, r.guild_id, r.channel_id), (mine.id, Id(10), Id(100)));
    assert_eq!((r.kind.as_str(), r.is_reply, r.via_tritone), ("answer", true, false));
    task.abort();
}

#[tokio::test]
async fn dm_own_message_not_recorded() {
    let live = live();
    let store = Arc::new(Store::open_in_memory().unwrap());
    let task = spawn_recorder(live.clone(), store.clone(), Arc::new(Config::default()));
    live.apply(GatewayEvent::MessageCreate(Box::new(msg(id_at(T0 + 1_000), 300, ME, "hi"))), T0);
    // A guild message after it proves the recorder got past the DM.
    live.apply(GatewayEvent::MessageCreate(Box::new(gmsg(2, 100, 10, ME, "x"))), T0);
    until_rows(&store, 1).await;
    let rows = store.activity_between(0, i64::MAX).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel_id, Id(100));
    task.abort();
}

#[test]
fn classify_topic_and_answer() {
    let live = live();
    let t = topics();
    let classify_in = |m: &Message| classify(m, live.channel(m.channel_id).as_ref(), &live, &t);
    // A keyword in the message itself, as a whole word, any case.
    assert_eq!(classify_in(&gmsg(1, 100, 10, ME, "Rust's borrow checker")), "rust");
    assert_eq!(classify_in(&gmsg(2, 100, 10, ME, "trusty old tool")), "other", "no partial-word match");
    // A keyword in the replied-to message.
    let q = gmsg(3, 100, 10, 2, "is cargo slow for you");
    live.apply(GatewayEvent::MessageCreate(Box::new(q.clone())), T0);
    assert_eq!(classify_in(&reply(gmsg(4, 100, 10, ME, "yes"), &q)), "rust");
    // A reply to a question.
    let q2 = gmsg(5, 100, 10, 2, "anyone know why this fails?");
    live.apply(GatewayEvent::MessageCreate(Box::new(q2.clone())), T0);
    assert_eq!(classify_in(&reply(gmsg(6, 100, 10, ME, "try this"), &q2)), "answer");
    // A reply in a channel whose name matches a topic.
    let other = gmsg(7, 101, 10, 2, "thanks all");
    live.apply(GatewayEvent::MessageCreate(Box::new(other.clone())), T0);
    assert_eq!(classify_in(&reply(gmsg(8, 101, 10, ME, "np"), &other)), "rust");
    assert_eq!(classify_in(&gmsg(9, 101, 10, ME, "np")), "other", "the channel name counts only for replies");
    // A reply to something not in the buffer: nothing to go on.
    let mut lost = gmsg(10, 100, 10, ME, "agreed");
    lost.reply_to = Some(Id(42));
    assert_eq!(classify_in(&lost), "other");
}

#[tokio::test]
async fn record_own_keeps_an_explicit_kind_over_the_echo() {
    let r = rig();
    let m = gmsg(1, 100, 10, ME, "about rust");
    // The gateway echo lands first, then the queue's record with a kind.
    assert!(record_own(&r.ctx.store, &r.ctx.live, &topics(), &m, None, None, false).unwrap());
    assert!(!record_own(&r.ctx.store, &r.ctx.live, &topics(), &m, None, Some("support"), true).unwrap());
    let row = &r.ctx.store.activity_between(0, i64::MAX).unwrap()[0];
    assert_eq!((row.kind.as_str(), row.via_tritone), ("support", true));
}

#[tokio::test]
async fn backfill_pages_with_delay_and_resumes() {
    let r = rig();
    let msgs: Vec<Message> = (0..30).map(|i| gmsg(i, 100, 10, ME, "x")).collect();
    index(&r, msgs);
    // Page 2 fails: the run stops with page 1 saved.
    r.api.fail_on_nth("search", 2, DiscordError::Http("down".into()));
    let gaps = pacer(4_000);
    assert!(backfill_with(&r.ctx, "backfill", &gaps).await.is_err());
    assert_eq!(searches(&r.api), vec![(SearchScope::Guild(Id(10)), 0), (SearchScope::Guild(Id(10)), 25)]);
    let t = r.api.times_of(is_search);
    assert_eq!(t[1] - t[0], 4_000, "a 3-6 s wait between pages");
    assert_eq!(r.ctx.store.activity_between(0, i64::MAX).unwrap().len(), 25);

    // The second run starts at offset 25, then does the other guild.
    let rep = backfill_with(&r.ctx, "backfill", &gaps).await.unwrap();
    let s = searches(&r.api);
    assert_eq!(s[2], (SearchScope::Guild(Id(10)), 25));
    assert_eq!(s[3], (SearchScope::Guild(Id(20)), 0));
    assert_eq!(s.len(), 4);
    assert_eq!((rep.guilds_done, rep.guilds_total, rep.recorded), (2, 2, 5));
    let rows = r.ctx.store.activity_between(0, i64::MAX).unwrap();
    assert_eq!(rows.len(), 30);
    assert!(rows.iter().all(|r| !r.via_tritone));

    // Done guilds are not searched again.
    backfill_with(&r.ctx, "backfill", &gaps).await.unwrap();
    assert_eq!(searches(&r.api).len(), 4);
}

#[tokio::test]
async fn backfill_only_searches_my_messages() {
    let r = rig();
    index(&r, vec![gmsg(1, 100, 10, ME, "mine"), gmsg(2, 100, 10, 2, "theirs")]);
    backfill_with(&r.ctx, "backfill", &pacer(3_000)).await.unwrap();
    let rows = r.ctx.store.activity_between(0, i64::MAX).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(r.api.calls().iter().all(|c| match c {
        Call::Search { query, .. } => query.author_id == Some(Id(ME)),
        _ => true,
    }));
}

#[tokio::test]
async fn backfill_waits_on_indexing() {
    let r = rig();
    index(&r, vec![gmsg(1, 100, 10, ME, "x")]);
    r.api.fail_on("search", DiscordError::Indexing { retry_after_s: 7 });
    let rep = backfill_with(&r.ctx, "backfill", &pacer(3_000)).await.unwrap();
    let s = searches(&r.api);
    assert_eq!(&s[..2], &[(SearchScope::Guild(Id(10)), 0), (SearchScope::Guild(Id(10)), 0)], "same page again");
    let t = r.api.times_of(is_search);
    assert!(t[1] - t[0] >= 7_000, "waited retry_after");
    assert_eq!(rep.recorded, 1);
}

#[tokio::test]
async fn backfill_stops_on_captcha() {
    let r = rig();
    index(&r, vec![gmsg(1, 100, 10, ME, "x"), gmsg(2, 200, 20, ME, "y")]);
    r.api.fail_on("search", DiscordError::Captcha);
    let e = backfill_with(&r.ctx, "backfill", &pacer(3_000)).await.unwrap_err();
    assert!(matches!(e, Error::Discord(DiscordError::Captcha)));
    assert_eq!(searches(&r.api).len(), 1, "nothing else is tried");
    assert_eq!(r.ctx.live.connection(), ConnState::Halted(DiscordError::Captcha));
}

#[tokio::test]
async fn backfill_switches_to_max_id_past_the_offset_limit() {
    let r = rig();
    // 10,005 of my messages; resume near the end of what offsets can reach.
    index(&r, (0..10_005).map(|i| gmsg(i, 100, 10, ME, "x")).collect());
    r.ctx.store.kv_set("backfill:10", r#"{"offset":9950,"max_id":null,"total":10005,"done":false}"#).unwrap();
    backfill_with(&r.ctx, "backfill", &pacer(3_000)).await.unwrap();
    let qs: Vec<SearchQuery> = r
        .api
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Search { scope: SearchScope::Guild(Id(10)), query } => Some(query),
            _ => None,
        })
        .collect();
    assert_eq!((qs[0].offset, qs[0].max_id), (9950, None));
    assert_eq!((qs[1].offset, qs[1].max_id), (9975, None));
    // Past 9975 the offset resets under a max_id cursor at the oldest id seen.
    assert_eq!((qs[2].offset, qs[2].max_id), (0, Some(id_at(T0 + 5_000))));
    assert_eq!(r.ctx.store.activity_between(0, i64::MAX).unwrap().len(), 55);
}

#[test]
fn my_activity_groups_and_links() {
    let r = rig();
    let s = &r.ctx.store;
    let row = |n: i64, guild: u64, channel: u64, kind: &str| ActivityRow {
        message_id: id_at(T0 + n * 1_000),
        guild_id: Id(guild),
        channel_id: Id(channel),
        created_at: T0 + n * 1_000,
        kind: kind.into(),
        is_reply: true,
        via_tritone: n % 2 == 0,
    };
    s.record_activity(&row(1, 10, 100, "answer")).unwrap();
    s.record_activity(&row(2, 10, 100, "answer")).unwrap();
    s.record_activity(&row(3, 10, 101, "rust")).unwrap();
    s.record_activity(&row(4, 20, 200, "other")).unwrap();
    s.record_activity(&row(-100, 20, 200, "other")).unwrap();
    let sum = my_activity(s, &r.ctx.live, T0, T0 + 10_000).unwrap();
    assert_eq!(sum.total, 4, "only rows in the period");
    let first = |v: &Vec<Count>| (v[0].label.clone(), v[0].count);
    assert_eq!(first(&sum.by_kind), ("answer".into(), 2));
    assert_eq!(first(&sum.by_server), ("G010".into(), 3));
    assert_eq!(first(&sum.by_channel), ("G010 › #C100".into(), 2));
    assert_eq!(sum.messages.len(), 4);
    let newest = &sum.messages[0];
    assert_eq!(newest.link, format!("https://discord.com/channels/20/200/{}", id_at(T0 + 4_000)));
    assert_eq!(newest.channel, "G020 › #C200");
    assert!(newest.via_tritone);
}

#[test]
fn my_activity_caps_links_at_50() {
    let r = rig();
    for n in 0..60 {
        let row = ActivityRow {
            message_id: id_at(T0 + n * 1_000),
            guild_id: Id(10),
            channel_id: Id(100),
            created_at: T0 + n * 1_000,
            kind: "other".into(),
            is_reply: false,
            via_tritone: false,
        };
        r.ctx.store.record_activity(&row).unwrap();
    }
    let sum = my_activity(&r.ctx.store, &r.ctx.live, T0, T0 + 100_000).unwrap();
    assert_eq!((sum.total, sum.messages.len()), (60, 50));
}

#[tokio::test]
async fn backfill_stops_when_tritone_is_halted_elsewhere() {
    let r = rig();
    index(&r, (0..30).map(|i| gmsg(i, 100, 10, ME, "x")).collect());
    // The gateway halts tritone while the backfill is between pages.
    r.ctx.live.halt(DiscordError::Locked);
    let e = backfill_with(&r.ctx, "backfill", &pacer(3_000)).await.unwrap_err();
    assert!(matches!(e, Error::Discord(DiscordError::Locked)));
    assert!(searches(&r.api).is_empty(), "no search while halted");
}
