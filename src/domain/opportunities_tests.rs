use std::collections::HashSet;
use std::sync::Arc;

use super::*;
use crate::discord::DiscordApi;
use crate::discord::fake::Call;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::ChannelKind;
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::BufferPolicy;
use crate::domain::send_queue::{JobSpec, SendQueue, Step};
use crate::domain::test_api::{Api, T0};
use crate::store::TargetKind;
use crate::test_support::{guild, id_at, msg, ready, text_channel};

const ME: u64 = 1;
const MIN: i64 = 60_000;

fn topics() -> Vec<Topic> {
    vec![Topic { kind: "rust".into(), keywords: vec!["rust".into(), "cargo".into()] }]
}

/// G010: C100 (full watch), C101 (normal), thread T150 under C100.
fn live() -> Arc<LiveState> {
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 24 * 60 * MIN }));
    let mut thread = text_channel(150, 10);
    thread.kind = ChannelKind::PublicThread;
    thread.parent_id = Some(Id(100));
    let snap = ready(ME, vec![guild(10)], vec![text_channel(100, 10), text_channel(101, 10), thread]);
    live.apply(GatewayEvent::Ready(Box::new(snap)), T0);
    live.set_buffered(HashSet::from([Id(100), Id(150)]));
    live
}

struct Rig {
    api: Arc<Api>,
    ctx: Ctx,
}

fn rig_with(live: Arc<LiveState>, store: Arc<Store>) -> Rig {
    let timer = Arc::new(FakeTimer::at(T0));
    let api = Arc::new(Api::new(timer.clone()));
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

fn rig() -> Rig {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.set_watch(Id(100), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    rig_with(live(), store)
}

/// A message `mins_ago` minutes before T0 in a guild channel.
fn gmsg(mins_ago: i64, channel: u64, author: u64, text: &str) -> Message {
    let mut m = msg(id_at(T0 - mins_ago * MIN), channel, author, text);
    m.guild_id = Some(Id(10));
    m
}

fn reply(mut m: Message, to: &Message) -> Message {
    m.reply_to = Some(to.id);
    m.reply_to_author = Some(to.author.id);
    m
}

/// Feeds a message through the live model and the scanner.
fn arrive(r: &Rig, m: &Message) {
    r.ctx.live.apply(GatewayEvent::MessageCreate(Box::new(m.clone())), m.timestamp_ms);
    let watch = r.ctx.store.all_watch().unwrap();
    let own = m.author.id == Id(ME);
    scan_message(&r.ctx.store, &r.ctx.live, &r.ctx.config.topics, &watch, m, own).unwrap();
}

fn rule_of(r: &Rig, m: &Message) -> Option<String> {
    r.ctx.store.opportunity(m.id).unwrap().map(|o| o.rule)
}

fn ids(opps: &[Opportunity]) -> Vec<Id> {
    opps.iter().map(|o| o.id).collect()
}

#[test]
fn question_rule_matches_shapes() {
    for q in [
        "does this work with tokio",
        "How do I build this",
        "anyone tried the new release",
        "Is there a guide for this",
        "can someone review my PR",
        "help: the build fails",
        "it fails on start?",
        "what",
    ] {
        assert!(is_question(q), "{q}");
    }
    for not in [
        "however it builds now",
        "helpful thread, thanks",
        "the build works",
        "see https://example.com/page?x=1",
        "isthere",
        "",
    ] {
        assert!(!is_question(not), "{not}");
    }
}

#[test]
fn topic_word_match_not_substring() {
    let r = rig();
    let hit = gmsg(60, 100, 2, "anything new in Rust today");
    let miss = gmsg(59, 100, 2, "the rustacean meetup was fun");
    arrive(&r, &hit);
    arrive(&r, &miss);
    assert_eq!(rule_of(&r, &hit).as_deref(), Some("topic:rust"));
    assert_eq!(rule_of(&r, &miss), None);
}

#[test]
fn bot_messages_ignored() {
    let r = rig();
    let mut m = gmsg(60, 100, 2, "how do I configure cargo?");
    m.author.bot = true;
    arrive(&r, &m);
    let mine = gmsg(59, 100, ME, "how do I configure this?");
    arrive(&r, &mine);
    assert_eq!(rule_of(&r, &m), None);
    assert_eq!(rule_of(&r, &mine), None, "never the user's own");
}

#[test]
fn only_full_watch_channels_scanned() {
    let r = rig();
    let normal = gmsg(60, 101, 2, "how does this work?");
    arrive(&r, &normal);
    assert_eq!(rule_of(&r, &normal), None);
    let thread = gmsg(60, 150, 2, "how does this work?");
    arrive(&r, &thread);
    assert_eq!(rule_of(&r, &thread).as_deref(), Some("question"), "a thread follows its parent's level");

    // A full-watch server covers its channels unless a channel says otherwise.
    r.ctx.store.set_watch(Id(10), TargetKind::Guild, Some(WatchLevel::Full)).unwrap();
    r.ctx.store.set_watch(Id(101), TargetKind::Channel, Some(WatchLevel::Muted)).unwrap();
    let muted = gmsg(59, 101, 2, "why is this?");
    arrive(&r, &muted);
    assert_eq!(rule_of(&r, &muted), None);
}

#[test]
fn my_thread_rule() {
    let r = rig();
    let mine = gmsg(90, 150, ME, "here is my take");
    arrive(&r, &mine);
    let in_my_thread = gmsg(80, 150, 2, "thanks, that helped");
    arrive(&r, &in_my_thread);
    assert_eq!(rule_of(&r, &in_my_thread).as_deref(), Some("my_thread"));

    let my_post = gmsg(70, 100, ME, "a note");
    arrive(&r, &my_post);
    let to_me = reply(gmsg(60, 100, 2, "nice note"), &my_post);
    arrive(&r, &to_me);
    assert_eq!(rule_of(&r, &to_me).as_deref(), Some("my_thread"));
}

#[tokio::test]
async fn young_question_waits() {
    let r = rig();
    let q = gmsg(10, 100, 2, "how do I do this?");
    let topic = gmsg(9, 100, 3, "cargo tip of the day");
    arrive(&r, &q);
    arrive(&r, &topic);
    let got = get_opportunities(&r.ctx, 15).await.unwrap();
    assert_eq!(ids(&got), [topic.id], "a question shows after 30 min, a topic at once");
    r.ctx.timer.sleep_until(T0 + 25 * MIN).await;
    let got = get_opportunities(&r.ctx, 15).await.unwrap();
    assert_eq!(ids(&got), [topic.id, q.id]);
}

#[tokio::test]
async fn old_question_expires() {
    let r = rig();
    let old = gmsg(49 * 60, 100, 2, "how do I do this?");
    let q = gmsg(47 * 60, 100, 2, "why does it fail?");
    arrive(&r, &old);
    arrive(&r, &q);
    r.api.inner.put_history(Id(100), vec![old.clone(), q.clone()]);
    let got = get_opportunities(&r.ctx, 15).await.unwrap();
    assert_eq!(ids(&got), [q.id], "older than 48 h is gone");
}

#[tokio::test]
async fn answered_question_dropped() {
    let r = rig();
    let context = gmsg(70, 100, 3, "morning");
    let q = gmsg(60, 100, 2, "how do I do this?");
    let open = gmsg(55, 100, 4, "why does it fail?");
    let own_follow_up = reply(gmsg(50, 100, 4, "more detail"), &open);
    let answer = reply(gmsg(45, 100, 3, "like this"), &q);
    for m in [&context, &q, &open, &own_follow_up, &answer] {
        arrive(&r, m);
    }
    let got = get_opportunities(&r.ctx, 15).await.unwrap();
    assert_eq!(ids(&got), [open.id], "the author's own follow-up doesn't count as an answer");
    assert_eq!(r.ctx.store.opportunity(q.id).unwrap().unwrap().status, OpportunityStatus::Answered);
    let o = &got[0];
    assert_eq!((o.rule, o.kind.as_str(), o.channel.as_str()), ("question", "answer", "G010 › #C100"));
    assert_eq!(o.context.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), [context.id.to_string(), q.id.to_string()]);
    assert_eq!(o.age_mins, 55);
    assert!(r.api.calls().is_empty(), "all from the buffer");
}

#[tokio::test]
async fn dismiss_persists() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.set_watch(Id(100), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    let r = rig_with(live(), store.clone());
    let topic = gmsg(5, 100, 2, "cargo question");
    arrive(&r, &topic);
    dismiss_opportunity(&r.ctx.store, topic.id, T0).unwrap();
    assert!(dismiss_opportunity(&r.ctx.store, Id(424_242), T0).is_err());
    arrive(&r, &topic);

    // A restart: same store, empty memory.
    let r2 = rig_with(live(), store);
    r2.api.inner.put_history(Id(100), vec![topic.clone()]);
    assert!(get_opportunities(&r2.ctx, 15).await.unwrap().is_empty());
}

#[tokio::test]
async fn opportunities_after_restart_use_rest_history() {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.set_watch(Id(100), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    let before = rig_with(live(), store.clone());
    let context = gmsg(70, 100, 3, "morning");
    let q = gmsg(60, 100, 2, "how do I do this?");
    let taken = gmsg(58, 100, 2, "is there a guide?");
    let answer = reply(gmsg(50, 100, 3, "yes, here"), &taken);
    for m in [&context, &q, &taken] {
        arrive(&before, m);
    }

    let r = rig_with(live(), store);
    assert!(r.ctx.live.recent(Id(100)).is_empty(), "the buffer starts empty");
    r.api.inner.put_history(Id(100), vec![context.clone(), q.clone(), taken.clone(), answer.clone()]);
    let got = get_opportunities(&r.ctx, 15).await.unwrap();
    assert_eq!(ids(&got), [q.id]);
    assert_eq!(got[0].context.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), [context.id.to_string()]);
    let arounds = r.api.calls().into_iter().filter(|c| matches!(c, Call::Around { .. })).count();
    assert_eq!(arounds, 2);

    // A message deleted since is dropped for good.
    let gone = gmsg(40, 100, 2, "what now?");
    arrive(&before, &gone);
    assert_eq!(ids(&get_opportunities(&r.ctx, 15).await.unwrap()), [q.id]);
    assert_eq!(r.ctx.store.opportunity(gone.id).unwrap().unwrap().status, OpportunityStatus::Gone);
}

#[tokio::test]
async fn replying_through_the_queue_or_an_app_marks_it_answered() {
    let r = rig();
    let a = gmsg(60, 100, 2, "how do I do this?");
    let b = gmsg(59, 100, 3, "why does it fail?");
    arrive(&r, &a);
    arrive(&r, &b);

    let spec = JobSpec::Send { channel: Id(100), content: "like this".into(), reply_to: Some(a.id), kind: None };
    r.ctx.queue.attach(r.api.clone() as Arc<dyn DiscordApi>);
    r.ctx.queue.enqueue(spec).unwrap();
    for _ in 0..100 {
        if matches!(r.ctx.queue.step().await, Step::Idle | Step::NotReady) {
            break;
        }
    }
    assert_eq!(r.ctx.store.opportunity(a.id).unwrap().unwrap().status, OpportunityStatus::Answered);

    // Typed in a Discord app: the gateway echo carries the reply.
    let typed = reply(gmsg(1, 100, ME, "try this"), &b);
    arrive(&r, &typed);
    assert_eq!(r.ctx.store.opportunity(b.id).unwrap().unwrap().status, OpportunityStatus::Answered);
}

#[tokio::test]
async fn scanner_task_stores_hits_from_live_events() {
    let r = rig();
    let handle = spawn_scanner(r.ctx.live.clone(), r.ctx.store.clone(), r.ctx.config.clone());
    let m = gmsg(5, 100, 2, "can someone explain this");
    r.ctx.live.apply(GatewayEvent::MessageCreate(Box::new(m.clone())), T0);
    for _ in 0..200 {
        if rule_of(&r, &m).is_some() {
            break;
        }
        tokio::task::yield_now().await;
    }
    handle.abort();
    assert_eq!(rule_of(&r, &m).as_deref(), Some("question"));
}

#[tokio::test]
async fn a_busy_thread_keeps_one_open_row() {
    let r = rig();
    arrive(&r, &gmsg(90, 150, ME, "here is my take"));
    let others: Vec<Message> = (0..5).map(|i| gmsg(80 - i, 150, 2 + i as u64, "more chat")).collect();
    for m in &others {
        arrive(&r, m);
    }
    let newest = others.last().unwrap();
    assert_eq!(ids(&get_opportunities(&r.ctx, 15).await.unwrap()), [newest.id], "only the thread's newest");
    let first = r.ctx.store.opportunity(others[0].id).unwrap().unwrap();
    assert_eq!(first.status, OpportunityStatus::Superseded);
}

#[tokio::test]
async fn my_message_closes_older_rows_in_that_channel() {
    let r = rig();
    let q = gmsg(60, 100, 2, "how do I do this?");
    let t = gmsg(55, 100, 3, "rust is neat");
    arrive(&r, &gmsg(90, 150, ME, "my take"));
    let in_thread = gmsg(50, 150, 4, "and then?");
    arrive(&r, &q);
    arrive(&r, &t);
    arrive(&r, &in_thread);
    // Written in a Discord app, no reply-quote.
    arrive(&r, &gmsg(45, 100, ME, "you can do it like this"));
    let later = gmsg(40, 100, 5, "why does it fail?");
    arrive(&r, &later);

    let status = |m: &Message| r.ctx.store.opportunity(m.id).unwrap().unwrap().status;
    assert_eq!((status(&q), status(&t)), (OpportunityStatus::Answered, OpportunityStatus::Answered));
    assert_eq!(status(&later), OpportunityStatus::Open, "newer than my message");
    assert_eq!(status(&in_thread), OpportunityStatus::Open, "another channel");
    assert_eq!(ids(&get_opportunities(&r.ctx, 15).await.unwrap()), [later.id, in_thread.id]);
}
