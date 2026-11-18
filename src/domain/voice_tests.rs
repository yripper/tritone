use std::sync::Arc;

use super::*;
use crate::config::Config;
use crate::discord::DiscordApi;
use crate::discord::fake::Call;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{ChannelKind, Message, SearchQuery, SearchScope};
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::BufferPolicy;
use crate::domain::send_queue::SendQueue;
use crate::domain::test_api::{Api, T0};
use crate::mcp::state::Ctx;
use crate::store::{ActivityRow, OwnMessageRow};
use crate::test_support::{dm, guild, id_at, msg, ready, text_channel};

const ME: u64 = 1;
const NOW: i64 = T0;
const DAY: i64 = 86_400_000;

/// G010 (C100, C101, thread T150 under C100), G020 (C200), G030 (C300), DM 400.
fn live() -> Arc<LiveState> {
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: DAY }));
    let mut thread = text_channel(150, 10);
    thread.kind = ChannelKind::PublicThread;
    thread.parent_id = Some(Id(100));
    let channels = vec![
        text_channel(100, 10),
        text_channel(101, 10),
        thread,
        text_channel(200, 20),
        text_channel(300, 30),
        dm(400, 2),
    ];
    let snap = ready(ME, vec![guild(10), guild(20), guild(30)], channels);
    live.apply(GatewayEvent::Ready(Box::new(snap)), T0);
    live
}

struct Rig {
    api: Arc<Api>,
    ctx: Ctx,
    pacer: SearchPacer,
}

fn rig() -> Rig {
    let timer = Arc::new(FakeTimer::at(T0));
    let api = Arc::new(Api::new(timer.clone()));
    let store = Arc::new(Store::open_in_memory().unwrap());
    let live = live();
    let config = Arc::new(Config::default());
    let queue = Arc::new(SendQueue::new(
        store.clone(),
        live.clone(),
        config.clone(),
        timer.clone(),
        Arc::new(SeqGaps::new(&[0])),
    ));
    let ctx = Ctx { store, live, api: api.clone() as Arc<dyn DiscordApi>, queue, config, timer };
    Rig { api, ctx, pacer: SearchPacer::new(Arc::new(SeqGaps::new(&[3_000]))) }
}

/// The `n`th own message (newest has the highest `n`) in a guild channel.
fn own_msg(n: i64, channel: u64, guild: u64, text: &str) -> Message {
    let mut m = msg(id_at(NOW - DAY + n * 1_000), channel, ME, text);
    m.guild_id = Some(Id(guild));
    m
}

fn index(r: &Rig, msgs: Vec<Message>) {
    r.api.inner.search_index.lock().unwrap().extend(msgs);
}

fn searches(api: &Api) -> Vec<(SearchScope, SearchQuery)> {
    api.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Search { scope, query } => Some((scope, query)),
            _ => None,
        })
        .collect()
}

fn own_rows(channel: u64, guild: Option<u64>, n: u64, from: i64) -> Vec<OwnMessageRow> {
    (0..n)
        .map(|i| OwnMessageRow {
            message_id: Id(10_000 + channel * 100 + i),
            channel_id: Id(channel),
            guild_id: guild.map(Id),
            created_at: from + 1 + i as i64,
        })
        .collect()
}

#[test]
fn staleness_by_count_or_age() {
    let age = STALE_AGE_SECS * 1000;
    assert!(!is_stale(NOW, 29, NOW));
    assert!(is_stale(NOW, 30, NOW));
    assert!(!is_stale(NOW - age + 1, 0, NOW));
    assert!(is_stale(NOW - age, 0, NOW));
}

#[test]
fn channel_with_no_notes_needs_notes_and_no_profile_needs_learning() {
    let r = rig();
    let ctx = voice_context(&r.ctx.store, &r.ctx.live, Id(100), NOW).unwrap();
    assert_eq!(
        ctx,
        VoiceContext {
            category: "general".into(),
            profile: None,
            notes: None,
            profile_stale: false,
            notes_stale: true,
            needs_learning: true,
        }
    );
}

#[test]
fn fresh_profile_and_notes_need_nothing_until_30_own_messages() {
    let r = rig();
    let s = &r.ctx.store;
    s.set_category(Id(100), "dev-community").unwrap();
    s.save_profile("dev-community", "short and concrete", NOW - DAY).unwrap();
    s.save_notes(Id(100), "answers with links", NOW - DAY).unwrap();
    let ctx = voice_context(s, &r.ctx.live, Id(100), NOW).unwrap();
    assert!(!ctx.needs_learning && !ctx.notes_stale && !ctx.profile_stale);
    assert_eq!(ctx.profile.as_deref(), Some("short and concrete"));

    s.record_own_messages(&own_rows(100, Some(10), 30, NOW - DAY), NOW).unwrap();
    let ctx = voice_context(s, &r.ctx.live, Id(100), NOW).unwrap();
    assert!(ctx.notes_stale && ctx.profile_stale && ctx.needs_learning);
    assert_eq!(stale_profiles(s, &r.ctx.live, NOW).unwrap(), ["dev-community"]);
}

#[test]
fn messages_sent_through_tritone_do_not_count_toward_staleness() {
    let r = rig();
    let s = &r.ctx.store;
    s.save_notes(Id(100), "n", NOW - DAY).unwrap();
    let rows = own_rows(100, Some(10), 30, NOW - DAY);
    s.record_own_messages(&rows, NOW).unwrap();
    let sent = ActivityRow {
        message_id: rows[0].message_id,
        guild_id: Id(10),
        channel_id: Id(100),
        created_at: rows[0].created_at,
        kind: "other".into(),
        is_reply: false,
        via_tritone: true,
    };
    s.record_activity(&sent).unwrap();
    assert!(!voice_context(s, &r.ctx.live, Id(100), NOW).unwrap().notes_stale);
}

#[test]
fn old_profile_is_stale_by_age() {
    let r = rig();
    let s = &r.ctx.store;
    s.save_profile("general", "x", NOW - STALE_AGE_SECS * 1000).unwrap();
    s.save_profile("work", "y", NOW).unwrap();
    assert_eq!(stale_profiles(s, &r.ctx.live, NOW).unwrap(), ["general"]);
}

#[test]
fn category_slug_rules() {
    for ok in ["general", "dev-community", "a_b", "x"] {
        assert!(valid_category(ok), "{ok}");
    }
    for bad in ["", "Work", "with space", "ñ", &"a".repeat(33)] {
        assert!(!valid_category(bad), "{bad}");
    }
}

#[test]
fn category_falls_back_to_server() {
    let r = rig();
    let (s, live) = (&r.ctx.store, &r.ctx.live);
    assert_eq!(category_of(s, live, Id(100), None).unwrap(), "general");
    s.set_category(Id(10), "work").unwrap();
    assert_eq!(category_of(s, live, Id(100), None).unwrap(), "work", "server fallback");
    assert_eq!(category_of(s, live, Id(150), None).unwrap(), "work", "thread → parent → server");
    s.set_category(Id(100), "dev-community").unwrap();
    assert_eq!(category_of(s, live, Id(100), None).unwrap(), "dev-community", "channel wins");
    assert_eq!(category_of(s, live, Id(150), None).unwrap(), "dev-community", "thread follows its parent");
    assert_eq!(category_of(s, live, Id(101), None).unwrap(), "work");
    // A channel the live model doesn't know uses the guild it was seen in.
    assert_eq!(category_of(s, live, Id(999), Some(Id(10))).unwrap(), "work");
    assert_eq!(category_of(s, live, Id(400), None).unwrap(), "general", "DMs default to general");

    // Staleness counts by the current category, so recategorizing moves activity with it.
    s.save_profile("work", "w", NOW - DAY).unwrap();
    s.record_own_messages(&own_rows(101, Some(10), 30, NOW - DAY), NOW).unwrap();
    assert_eq!(stale_profiles(s, live, NOW).unwrap(), ["work"]);
    s.set_category(Id(101), "friends").unwrap();
    assert!(stale_profiles(s, live, NOW).unwrap().is_empty());
}

#[test]
fn own_texts_skip_sent_others_and_blank() {
    let msgs = vec![
        own_msg(5, 100, 10, "mine"),
        own_msg(4, 100, 10, "sent by tritone"),
        msg(id_at(NOW - 3_000), 100, 2, "theirs"),
        own_msg(2, 100, 10, "   "),
    ];
    let sent: HashSet<Id> = [msgs[1].id].into();
    let got: Vec<&str> = own_texts(&msgs, Id(ME), &sent).iter().map(|m| m.content.as_str()).collect();
    assert_eq!(got, ["mine"]);
}

#[tokio::test]
async fn get_voice_samples_from_search() {
    let r = rig();
    let mut msgs: Vec<Message> = (0..120).map(|i| own_msg(i, 100, 10, "fake text")).collect();
    msgs.push(own_msg(500, 101, 10, "another channel"));
    index(&r, msgs);
    let newest = own_msg(119, 100, 10, "").id;
    r.ctx
        .store
        .record_activity(&ActivityRow {
            message_id: newest,
            guild_id: Id(10),
            channel_id: Id(100),
            created_at: NOW,
            kind: "other".into(),
            is_reply: false,
            via_tritone: true,
        })
        .unwrap();

    let got = own_messages(&r.ctx, &r.pacer, Id(100), CHAT_SAMPLE).await.unwrap();
    assert_eq!(got.len(), 60);
    assert!(got.iter().all(|m| m.channel_id == Id(100)));
    assert_eq!(got[0].id, own_msg(118, 100, 10, "").id, "newest first, the tritone send excluded");

    let s = searches(&r.api);
    assert_eq!(s.len(), 3, "25 per page");
    for (i, (scope, q)) in s.iter().enumerate() {
        assert_eq!(*scope, SearchScope::Guild(Id(10)));
        assert_eq!((q.author_id, q.channel_id, q.offset), (Some(Id(ME)), Some(Id(100)), 25 * i as u32));
    }
    assert_eq!(r.api.times_of(|c| matches!(c, Call::Search { .. })), [0, 3_000, 6_000], "paced");
    assert_eq!(r.ctx.store.own_count_in_channel(Id(100), 0).unwrap(), 74, "75 seen, 1 sent by tritone");
}

#[tokio::test]
async fn dm_samples_search_the_channel_itself() {
    let r = rig();
    index(&r, vec![msg(id_at(NOW - 1_000), 400, ME, "hey")]);
    let got = own_messages(&r.ctx, &r.pacer, Id(400), CHAT_SAMPLE).await.unwrap();
    assert_eq!(got.len(), 1);
    let (scope, q) = &searches(&r.api)[0];
    assert_eq!((*scope, q.channel_id), (SearchScope::Channel(Id(400)), None));
}

#[tokio::test]
async fn category_sample_reads_at_most_three_other_channels() {
    let r = rig();
    let s = &r.ctx.store;
    for c in [100, 101, 200, 300] {
        let g = c / 10;
        s.set_category(Id(g), "work").unwrap();
        index(&r, (0..30).map(|i| own_msg(i, c, g, "fake text")).collect());
    }
    // Channels the user wrote in, newest first: C300, C200, C101, C100.
    let rows: Vec<OwnMessageRow> = [100u64, 101, 200, 300]
        .iter()
        .enumerate()
        .map(|(i, c)| OwnMessageRow {
            message_id: Id(50_000 + c),
            channel_id: Id(*c),
            guild_id: Some(Id(c / 10)),
            created_at: NOW - DAY + i as i64,
        })
        .collect();
    s.record_own_messages(&rows, NOW).unwrap();

    let got = own_messages_in_category(&r.ctx, &r.pacer, "work", Id(101), CATEGORY_SAMPLE).await.unwrap();
    assert_eq!(got.len(), 40);
    let channels: Vec<Option<Id>> = searches(&r.api).iter().map(|(_, q)| q.channel_id).collect();
    assert_eq!(channels, [Some(Id(300)), Some(Id(300)), Some(Id(200))], "C101 itself skipped, stops at 40");
}

#[tokio::test]
async fn category_sample_falls_back_to_server_wide_search() {
    let r = rig();
    r.ctx.store.set_category(Id(20), "work").unwrap();
    index(&r, (0..5).map(|i| own_msg(i, 200, 20, "fake text")).collect());
    let got = own_messages_in_category(&r.ctx, &r.pacer, "work", Id(100), CATEGORY_SAMPLE).await.unwrap();
    assert_eq!(got.len(), 5);
    let (scope, q) = &searches(&r.api)[0];
    assert_eq!((*scope, q.channel_id), (SearchScope::Guild(Id(20)), None));
}

#[tokio::test]
async fn get_voice_fetches_only_what_is_stale() {
    let r = rig();
    index(&r, (0..3).map(|i| own_msg(i, 100, 10, "fake text")).collect());
    let report = get_voice(&r.ctx, &r.pacer, Id(100)).await.unwrap();
    assert!(report.context.needs_learning);
    assert_eq!(report.my_messages_in_channel.as_ref().map(Vec::len), Some(3));
    assert!(report.my_messages_in_category.is_some());

    r.ctx.store.save_profile("general", "p", NOW).unwrap();
    r.ctx.store.save_notes(Id(100), "n", NOW).unwrap();
    let before = searches(&r.api).len();
    let report = get_voice(&r.ctx, &r.pacer, Id(100)).await.unwrap();
    assert!(report.my_messages_in_channel.is_none() && report.my_messages_in_category.is_none());
    assert_eq!(searches(&r.api).len(), before, "fresh voice needs no search");
}

#[test]
fn save_checks_slug_and_length() {
    let r = rig();
    assert!(save_profile(&r.ctx.store, "Work", "x", NOW).is_err());
    assert!(save_profile(&r.ctx.store, "work", "", NOW).is_err());
    assert!(save_profile(&r.ctx.store, "work", &"x".repeat(PROFILE_MAX + 1), NOW).is_err());
    save_profile(&r.ctx.store, "work", "fine", NOW).unwrap();
    assert!(save_chat_notes(&r.ctx.store, Id(100), &"x".repeat(NOTES_MAX + 1), NOW).is_err());
}
