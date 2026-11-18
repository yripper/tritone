use std::sync::Arc;

use super::*;
use crate::config::Config;
use crate::discord::DiscordApi;
use crate::discord::fake::Call;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{Channel, DiscordError, Message, MuteTarget, MuteUntil};
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, ConnState};
use crate::domain::send_queue::SendQueue;
use crate::domain::test_api::{Api, T0};
use crate::store::{ActivityRow, TargetKind, WatchLevel};
use crate::test_support::{guild, id_at, msg, ready, settings, text_channel};

const ME: u64 = 1;
const DAY: i64 = 86_400_000;

/// A channel whose newest message is unread (no read state).
fn unread(n: u64, g: u64) -> Channel {
    let mut c = text_channel(n, g);
    c.last_message_id = Some(id_at(T0 - DAY + n as i64));
    c
}

/// G010 (C100 unread), G020 (C200 unread), G030 (C300..C320: 21 unread), G040 (C400, owned by me).
fn live() -> Arc<LiveState> {
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: DAY }));
    let mut channels = vec![unread(100, 10), unread(200, 20), text_channel(400, 40)];
    channels.extend((300..321).map(|n| unread(n, 30)));
    let mut owned = guild(40);
    owned.owner_id = Id(ME);
    let mut snap = ready(ME, vec![guild(10), guild(20), guild(30), owned], channels);
    snap.settings = vec![settings(Some(20), MuteUntil::Forever)];
    live.apply(GatewayEvent::Ready(Box::new(snap)), T0);
    live
}

struct Rig {
    api: Arc<Api>,
    ctx: Ctx,
    pacer: SearchPacer,
    gaps: SeqGaps,
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
    Rig { api, ctx, pacer: SearchPacer::new(Arc::new(SeqGaps::new(&[3_000]))), gaps: SeqGaps::new(&[2_000]) }
}

/// My post in a guild channel, `days` ago.
fn my_post(days: i64, channel: u64, g: u64) -> Message {
    let mut m = msg(id_at(T0 - days * DAY), channel, ME, "fake text");
    m.guild_id = Some(Id(g));
    m
}

fn index(r: &Rig, msgs: Vec<Message>) {
    r.api.inner.search_index.lock().unwrap().extend(msgs);
}

fn searched_guilds(api: &Api) -> Vec<Id> {
    api.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Search { scope: SearchScope::Guild(g), .. } => Some(g),
            _ => None,
        })
        .collect()
}

fn entry(plan: &CleanupPlan, g: u64) -> &ServerEntry {
    plan.servers.iter().find(|s| s.id == Id(g)).unwrap()
}

fn items(plan: &CleanupPlan, g: u64) -> Vec<String> {
    entry(plan, g).items.iter().map(|i| i.item.clone()).collect()
}

#[tokio::test]
async fn plan_suggests_leave_for_stale_guild() {
    let r = rig();
    index(&r, vec![my_post(200, 100, 10), my_post(10, 200, 20), my_post(90, 300, 30)]);
    let plan = cleanup_plan_with(&r.ctx, &r.pacer).await.unwrap();

    let g10 = entry(&plan, 10);
    assert_eq!(g10.suggestion, Some(Suggestion::Leave));
    assert_eq!(g10.unread_channels, 1);
    assert!(g10.my_last_post.is_some());
    assert_eq!(items(&plan, 10), ["leave:10", "read:10"]);

    let g20 = entry(&plan, 20);
    assert_eq!(g20.suggestion, None, "posted 10 days ago");
    assert!(g20.muted_in_discord);

    // 21 unread channels and nothing for 90 days: mute, not leave.
    assert_eq!(entry(&plan, 30).suggestion, Some(Suggestion::Mute));
    assert_eq!(items(&plan, 30), ["mute:30", "read:30"]);

    // Never posted, but I own it: never leave.
    assert_eq!(entry(&plan, 40).suggestion, None);
    assert_eq!(plan.servers[0].id, Id(10), "suggestions first");
    assert_eq!(r.api.times_of(|c| matches!(c, Call::Search { .. })), [0, 3_000, 6_000, 9_000], "paced");
}

#[tokio::test]
async fn plan_never_suggests_leaving_full_watch() {
    let r = rig();
    r.ctx.store.set_watch(Id(100), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    r.ctx.store.set_watch(Id(20), TargetKind::Guild, Some(WatchLevel::Full)).unwrap();
    let plan = cleanup_plan_with(&r.ctx, &r.pacer).await.unwrap();
    for g in [10, 20] {
        let e = entry(&plan, g);
        assert!(e.full_watch, "{g}");
        assert_eq!(e.suggestion, None, "never posted, but full watch: {g}");
        assert!(!items(&plan, g).iter().any(|i| i.starts_with("leave:")));
    }
}

#[tokio::test]
async fn plan_uses_the_log_and_caches_lookups_for_a_day() {
    let r = rig();
    let row = ActivityRow {
        message_id: id_at(T0 - 5 * DAY),
        guild_id: Id(10),
        channel_id: Id(100),
        created_at: T0 - 5 * DAY,
        kind: "other".into(),
        is_reply: false,
        via_tritone: false,
    };
    r.ctx.store.record_activity(&row).unwrap();
    cleanup_plan_with(&r.ctx, &r.pacer).await.unwrap();
    let first = searched_guilds(&r.api);
    assert!(!first.contains(&Id(10)), "a post 5 days ago in the log needs no search");
    assert_eq!(first.len(), 3);

    cleanup_plan_with(&r.ctx, &r.pacer).await.unwrap();
    assert_eq!(searched_guilds(&r.api).len(), 3, "cached");

    r.ctx.timer.sleep_until(T0 + 25 * 3_600_000).await;
    cleanup_plan_with(&r.ctx, &r.pacer).await.unwrap();
    assert_eq!(searched_guilds(&r.api).len(), 6, "stale after 24 h");
}

#[tokio::test]
async fn plan_looks_up_a_few_servers_per_call() {
    let r = rig();
    let mut guilds = Vec::new();
    let mut channels = Vec::new();
    for g in 50..(50 + MAX_LOOKUPS_PER_PLAN as u64 + 2) {
        guilds.push(guild(g));
        channels.push(text_channel(g * 10, g));
    }
    for (g, cs) in guilds.into_iter().zip(channels) {
        r.ctx.live.apply(GatewayEvent::GuildUpsert(Box::new(g), vec![cs]), T0);
    }
    let plan = cleanup_plan_with(&r.ctx, &r.pacer).await.unwrap();
    assert_eq!(searched_guilds(&r.api).len(), MAX_LOOKUPS_PER_PLAN);
    // 4 servers from READY plus MAX + 2 new ones, none with a known post.
    assert_eq!(plan.lookups_pending, 4 + 2);
    let unknown = plan.servers.iter().filter(|s| !s.last_post_known).count();
    assert_eq!(unknown, plan.lookups_pending);
    assert!(plan.servers.iter().filter(|s| !s.last_post_known).all(|s| s.suggestion.is_none()));
}

#[tokio::test]
async fn noisy_server_i_post_in_gets_channel_mutes() {
    let r = rig();
    // A recent post in C300: no server-level suggestion, so quiet channels I never post in are offered.
    index(&r, vec![my_post(3, 300, 30)]);
    let plan = cleanup_plan_with(&r.ctx, &r.pacer).await.unwrap();
    let e = entry(&plan, 30);
    assert_eq!(e.suggestion, None);
    let mutes: Vec<String> = items(&plan, 30).into_iter().filter(|i| i.starts_with("mute:30/")).collect();
    assert_eq!(mutes.len(), CHANNEL_MUTE_SUGGESTIONS);
    assert!(!mutes.contains(&"mute:30/300".to_string()), "a channel I post in");
}

#[tokio::test]
async fn apply_runs_only_listed_items() {
    let r = rig();
    let given = ["mute:30/301", "leave:10", "nonsense", "mute:20", "leave:10"].map(String::from);
    let out = cleanup_apply_with(&r.ctx, &given, &r.gaps).await.unwrap();
    let writes: Vec<Call> =
        r.api.calls().into_iter().filter(|c| matches!(c, Call::LeaveGuild(_) | Call::SetMute { .. })).collect();
    assert_eq!(
        writes,
        [
            Call::SetMute { target: MuteTarget::Channel { guild: Some(Id(30)), channel: Id(301) }, muted: true },
            Call::LeaveGuild(Id(10)),
            Call::SetMute { target: MuteTarget::Guild(Id(20)), muted: true },
        ]
    );
    let status: Vec<ApplyStatus> = out.iter().map(|o| o.status).collect();
    assert_eq!(
        status,
        [ApplyStatus::Done, ApplyStatus::Done, ApplyStatus::Failed, ApplyStatus::Done, ApplyStatus::Skipped]
    );
}

#[tokio::test]
async fn apply_read_acks_the_servers_unread_channels() {
    let r = rig();
    let out = cleanup_apply_with(&r.ctx, &["read:30".to_string()], &r.gaps).await.unwrap();
    assert_eq!(out[0].status, ApplyStatus::Done);
    let acked: usize = r
        .api
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::AckBulk(items) => Some(items.len()),
            _ => None,
        })
        .sum();
    assert_eq!(acked, 21);
    assert!(!r.ctx.live.is_unread(Id(300)));
}

#[tokio::test]
async fn apply_spaces_actions() {
    let r = rig();
    let given = ["mute:10", "mute:30", "read:20"].map(String::from);
    cleanup_apply_with(&r.ctx, &given, &r.gaps).await.unwrap();
    let times = r.api.times_of(|c| matches!(c, Call::SetMute { .. } | Call::AckBulk(_)));
    assert_eq!(times, [0, 2_000, 4_000]);
}

#[tokio::test]
async fn apply_stops_on_halt() {
    let r = rig();
    r.api.fail_on("set_mute", DiscordError::Captcha);
    let given = ["leave:10", "mute:30", "leave:20"].map(String::from);
    let out = cleanup_apply_with(&r.ctx, &given, &r.gaps).await.unwrap();
    let status: Vec<ApplyStatus> = out.iter().map(|o| o.status).collect();
    assert_eq!(status, [ApplyStatus::Done, ApplyStatus::Failed, ApplyStatus::Skipped]);
    assert_eq!(r.ctx.live.connection(), ConnState::Halted(DiscordError::Captcha));
    assert!(!r.api.calls().contains(&Call::LeaveGuild(Id(20))));
}
