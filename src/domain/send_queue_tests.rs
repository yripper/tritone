use super::*;
use crate::config::Config;
use crate::discord::fake::Call;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{ChannelKind, DiscordError, Id};
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, ConnState};
use crate::domain::test_api::{Api, T0};
use crate::test_support::{guild, ready, text_channel};

const CH: Id = Id(100);
const FORUM: Id = Id(150);

struct Rig {
    api: Arc<Api>,
    store: Arc<Store>,
    live: Arc<LiveState>,
    q: SendQueue,
}

fn rig(gaps: &[u64]) -> Rig {
    let timer = Arc::new(FakeTimer::at(T0));
    let api = Arc::new(Api::new(timer.clone()));
    let store = Arc::new(Store::open_in_memory().unwrap());
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 86_400_000 }));
    let mut forum = text_channel(FORUM.0, 10);
    forum.kind = ChannelKind::Forum;
    live.apply(GatewayEvent::Ready(Box::new(ready(1, vec![guild(10)], vec![text_channel(CH.0, 10), forum]))), T0);
    let q = SendQueue::new(
        store.clone(),
        live.clone(),
        Arc::new(Config::default()),
        timer.clone(),
        Arc::new(SeqGaps::new(gaps)),
    );
    q.attach(api.clone());
    Rig { api, store, live, q }
}

fn send(content: &str) -> JobSpec {
    JobSpec::Send { channel: CH, content: content.into(), reply_to: None, kind: None }
}

async fn drain(q: &SendQueue) {
    for _ in 0..1_000 {
        if matches!(q.step().await, Step::Idle | Step::NotReady) {
            return;
        }
    }
    panic!("queue never went idle");
}

fn job(q: &SendQueue, id: u64) -> JobView {
    q.status(Some(id)).unwrap().jobs.remove(0)
}

fn is_send(c: &Call) -> bool {
    matches!(c, Call::Send { .. })
}

fn is_typing(c: &Call) -> bool {
    matches!(c, Call::Typing(_))
}

fn send_nonces(api: &Api) -> Vec<Id> {
    api.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Send { msg, .. } => Some(msg.nonce),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn typing_precedes_send() {
    let r = rig(&[20_000]);
    r.q.enqueue(send("hello")).unwrap();
    assert!(r.api.calls().is_empty(), "enqueue returns before anything goes out");
    drain(&r.q).await;
    let calls = r.api.calls();
    assert_eq!(calls[0], Call::Typing(CH));
    assert!(matches!(&calls[1], Call::Send { channel, msg } if *channel == CH && msg.content == "hello"));
}

#[tokio::test]
async fn typing_wait_between_2_and_5s() {
    for (draw, want) in [(1_000, TYPING_MIN_MS), (3_000, 3_000), (90_000, TYPING_MAX_MS)] {
        let r = rig(&[draw]);
        r.q.enqueue(send("hi")).unwrap();
        drain(&r.q).await;
        let typed = r.api.times_of(is_typing)[0];
        let sent = r.api.times_of(is_send)[0];
        assert_eq!((sent - typed) as u64, want, "draw {draw}");
    }
}

#[tokio::test]
async fn gap_between_sends_20_to_60s() {
    let r = rig(&[30_000]);
    for t in ["one", "two", "three"] {
        r.q.enqueue(send(t)).unwrap();
    }
    drain(&r.q).await;
    let sends = r.api.times_of(is_send);
    assert_eq!(sends.len(), 3);
    for w in sends.windows(2) {
        let d = (w[1] - w[0]) as u64;
        assert!((MIN_GAP_MS..=MAX_GAP_MS + TYPING_MAX_MS).contains(&d), "gap {d}");
    }
    // Gap counts from the previous send, then typing: 30 s + 5 s.
    assert_eq!(sends, [5_000, 40_000, 75_000]);
}

#[tokio::test]
async fn etas_simulate_the_queue() {
    let r = rig(&[20_000, 30_000, 45_000]);
    let a = r.q.enqueue(send("one")).unwrap();
    let b = r.q.enqueue(send("two")).unwrap();
    assert_eq!((a.position, a.eta_secs), (1, 0));
    assert_eq!((b.position, b.eta_secs), (2, 30));
}

#[tokio::test]
async fn hourly_cap_30() {
    let r = rig(&[20_000]);
    let mut last = None;
    for i in 0..31 {
        last = Some(r.q.enqueue(JobSpec::React { channel: CH, message: Id(5), emoji: format!("e{i}") }).unwrap());
    }
    assert_eq!(last.unwrap().eta_secs, 3_600, "eta reflects the cap");
    drain(&r.q).await;
    let reacts = r.api.times_of(|c| matches!(c, Call::React { .. }));
    assert_eq!(reacts.len(), 31);
    assert_eq!(reacts[29], 29 * 20_000);
    assert_eq!(reacts[30], 3_600_000, "the 31st waits until the 1st is an hour old");
    assert!(r.q.status(None).unwrap().writes_last_hour <= HOURLY_CAP);
}

#[tokio::test]
async fn retry_reuses_nonce() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::RateLimited { retry_after_s: 10 });
    r.q.enqueue(send("hi")).unwrap();
    drain(&r.q).await;
    let nonces = send_nonces(&r.api);
    assert_eq!(nonces.len(), 2, "one retry");
    assert_eq!(nonces[0], nonces[1], "same nonce, so Discord can't double-post");
    let j = job(&r.q, 1);
    assert_eq!(j.status, JobStatus::Sent);
    let sends = r.api.times_of(is_send);
    assert!(sends[1] - sends[0] >= 10_000, "waited Retry-After");
}

#[tokio::test]
async fn second_rate_limit_fails_the_job_and_the_queue_goes_on() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::RateLimited { retry_after_s: 5 });
    r.api.fail_on("send", DiscordError::RateLimited { retry_after_s: 5 });
    r.q.enqueue(send("one")).unwrap();
    r.q.enqueue(send("two")).unwrap();
    drain(&r.q).await;
    assert_eq!(job(&r.q, 1).status, JobStatus::Failed);
    assert_eq!(job(&r.q, 2).status, JobStatus::Sent);
}

#[tokio::test]
async fn other_errors_fail_only_that_job_without_retry() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::Forbidden);
    r.q.enqueue(send("one")).unwrap();
    r.q.enqueue(send("two")).unwrap();
    drain(&r.q).await;
    assert_eq!(job(&r.q, 1).status, JobStatus::Failed);
    assert!(job(&r.q, 1).error.unwrap().contains("no access"));
    assert_eq!(job(&r.q, 2).status, JobStatus::Sent);
    assert_eq!(send_nonces(&r.api).len(), 2, "no retry without Retry-After");
}

#[tokio::test]
async fn captcha_on_send_stops_queue() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::Captcha);
    r.q.enqueue(send("one")).unwrap();
    r.q.enqueue(send("two")).unwrap();
    drain(&r.q).await;
    let one = job(&r.q, 1);
    assert_eq!(one.status, JobStatus::Failed);
    assert!(one.error.unwrap().contains("CAPTCHA"));
    assert_eq!(job(&r.q, 2).status, JobStatus::Queued, "the next job stays queued");
    let calls = r.api.calls().len();
    for _ in 0..3 {
        assert_eq!(r.q.step().await, Step::Idle);
    }
    assert_eq!(r.api.calls().len(), calls, "no further calls");
    let st = r.q.status(None).unwrap();
    assert!(st.paused.unwrap().contains("CAPTCHA"));
    // C2: all of tritone stops, not just the queue.
    assert_eq!(r.live.connection(), ConnState::Halted(DiscordError::Captcha));
}

#[tokio::test]
async fn halting_error_on_typing_also_halts_live() {
    let r = rig(&[20_000]);
    r.api.fail_on("typing", DiscordError::Locked);
    r.q.enqueue(send("one")).unwrap();
    drain(&r.q).await;
    assert_eq!(send_nonces(&r.api).len(), 0, "never sends without typing first");
    assert_eq!(r.live.connection(), ConnState::Halted(DiscordError::Locked));
}

#[tokio::test]
async fn needs_login_keeps_an_existing_rejection() {
    let r = rig(&[20_000]);
    // The REST client already rejected the token: the queue must not overwrite that.
    r.live.reject_token("dead");
    r.q.enqueue(send("one")).unwrap();
    drain(&r.q).await;
    assert!(r.q.status(None).unwrap().paused.is_some(), "a halted live pauses the queue");
    assert!(r.api.calls().is_empty());
    assert!(!r.live.try_login_with("dead"), "the dead token stays rejected");
}

#[tokio::test]
async fn needs_login_from_a_send_halts_live() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::NeedsLogin);
    r.q.enqueue(send("one")).unwrap();
    drain(&r.q).await;
    assert_eq!(r.live.connection(), ConnState::Halted(DiscordError::NeedsLogin));
}

#[tokio::test]
async fn resume_refuses_while_needs_login() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::NeedsLogin);
    r.q.enqueue(send("one")).unwrap();
    drain(&r.q).await;
    let e = r.q.resume().unwrap_err();
    assert!(e.to_string().contains("tritone login"), "{e}");
    assert!(r.q.status(None).unwrap().paused.is_some());
}

#[tokio::test]
async fn resume_refuses_after_the_gateway_closed_the_session() {
    let r = rig(&[20_000]);
    let closed = DiscordError::GatewayClosed { code: 4011 };
    r.live.halt(closed.clone());
    let e = r.q.resume().unwrap_err();
    assert!(e.to_string().contains("restart tritone"), "{e}");
    assert_eq!(r.live.connection(), ConnState::Halted(closed), "only a restart lifts it");
}

#[tokio::test]
async fn resume_refuses_after_discord_kept_refusing_the_session() {
    let r = rig(&[20_000]);
    r.live.halt(DiscordError::SessionRefused);
    let e = r.q.resume().unwrap_err();
    assert!(e.to_string().contains("restart tritone"), "{e}");
    assert_eq!(r.live.connection(), ConnState::Halted(DiscordError::SessionRefused));
}

#[tokio::test]
async fn resume_after_captcha_clears_the_halt_and_continues() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::Captcha);
    r.q.enqueue(send("one")).unwrap();
    r.q.enqueue(send("two")).unwrap();
    drain(&r.q).await;
    r.q.resume().unwrap();
    assert!(matches!(r.live.connection(), ConnState::Ready { .. }), "live halt lifted");
    assert!(r.q.status(None).unwrap().paused.is_none());
    drain(&r.q).await;
    assert_eq!(job(&r.q, 1).status, JobStatus::Failed, "the failed job is never retried");
    assert_eq!(job(&r.q, 2).status, JobStatus::Sent);
}

#[tokio::test]
async fn resume_waits_for_a_ready_connection() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::Captcha);
    r.q.enqueue(send("one")).unwrap();
    drain(&r.q).await;
    r.live.apply(GatewayEvent::Disconnected, T0);
    assert!(r.q.resume().is_err(), "not ready yet");
    assert!(r.q.status(None).unwrap().paused.is_some());
}

#[tokio::test]
async fn not_ready_stays_idle() {
    let r = rig(&[20_000]);
    r.live.apply(GatewayEvent::Disconnected, T0);
    r.q.enqueue(send("one")).unwrap();
    assert_eq!(r.q.step().await, Step::NotReady);
    assert!(r.api.calls().is_empty());
    assert!(r.q.status(None).unwrap().paused.is_none(), "a reconnect is not a pause");
}

#[tokio::test]
async fn send_acks_channel_after() {
    let r = rig(&[20_000]);
    r.q.enqueue(send("hi")).unwrap();
    drain(&r.q).await;
    let j = job(&r.q, 1);
    let sent: Id = j.sent_message_id.clone().unwrap().parse().unwrap();
    let calls = r.api.calls();
    let send_at = calls.iter().position(is_send).unwrap();
    assert_eq!(calls[send_at + 1], Call::Ack { channel: CH, message: sent });
    assert_eq!(r.live.read_state(CH).last_acked, Some(sent));
    assert_eq!(j.link.unwrap(), format!("https://discord.com/channels/10/100/{sent}"));
}

#[tokio::test]
async fn ack_error_never_fails_a_sent_message() {
    let r = rig(&[20_000]);
    r.api.fail_on("ack", DiscordError::Http("flaky".into()));
    r.q.enqueue(send("hi")).unwrap();
    drain(&r.q).await;
    let j = job(&r.q, 1);
    assert_eq!(j.status, JobStatus::Sent);
    assert!(j.sent_message_id.is_some());
    assert!(j.ack_error.unwrap().contains("flaky"));
}

#[tokio::test]
async fn send_records_activity_via_tritone() {
    let r = rig(&[20_000]);
    r.q.enqueue(JobSpec::Send {
        channel: CH,
        content: "hi".into(),
        reply_to: Some(Id(7)),
        kind: Some("support".into()),
    })
    .unwrap();
    drain(&r.q).await;
    let rows = r.store.activity_between(0, i64::MAX).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].via_tritone && rows[0].is_reply);
    assert_eq!((rows[0].guild_id, rows[0].channel_id, rows[0].kind.as_str()), (Id(10), CH, "support"));
}

#[tokio::test]
async fn reaction_skips_typing() {
    let r = rig(&[20_000]);
    r.q.enqueue(JobSpec::React { channel: CH, message: Id(5), emoji: "👍".into() }).unwrap();
    drain(&r.q).await;
    assert_eq!(r.api.calls(), vec![Call::React { channel: CH, message: Id(5), emoji: "👍".into() }]);
    assert_eq!(job(&r.q, 1).status, JobStatus::Sent);
}

#[tokio::test]
async fn thread_with_content_types_in_the_new_thread() {
    let r = rig(&[20_000]);
    r.q.enqueue(JobSpec::Thread {
        channel: CH,
        from_message: Some(Id(5)),
        name: "T".into(),
        content: Some("hi".into()),
    })
    .unwrap();
    drain(&r.q).await;
    let calls = r.api.calls();
    assert_eq!(calls[0], Call::CreateThread { channel: CH, from_message: Some(Id(5)), name: "T".into() });
    assert_eq!(calls[1], Call::Typing(Id(5)));
    assert!(matches!(&calls[2], Call::Send { channel, .. } if *channel == Id(5)));
    let j = job(&r.q, 1);
    assert_eq!(j.status, JobStatus::Sent);
    assert_eq!(j.thread_id.as_deref(), Some("5"));
}

#[tokio::test]
async fn thread_is_not_created_twice_on_a_retry() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::RateLimited { retry_after_s: 1 });
    r.q.enqueue(JobSpec::Thread { channel: CH, from_message: None, name: "T".into(), content: Some("hi".into()) })
        .unwrap();
    drain(&r.q).await;
    let creates = r.api.calls().iter().filter(|c| matches!(c, Call::CreateThread { .. })).count();
    assert_eq!(creates, 1);
    assert_eq!(job(&r.q, 1).status, JobStatus::Sent);
}

#[tokio::test]
async fn forum_post_not_retried() {
    let r = rig(&[20_000]);
    r.api.fail_on("create_forum_post", DiscordError::RateLimited { retry_after_s: 30 });
    r.q.enqueue(JobSpec::ForumPost { forum: FORUM, name: "Title".into(), content: "body".into() }).unwrap();
    r.q.enqueue(send("next")).unwrap();
    drain(&r.q).await;
    let posts = r.api.times_of(|c| matches!(c, Call::CreateForumPost { .. }));
    assert_eq!(posts.len(), 1, "the post carries no nonce: never retried");
    let j = job(&r.q, 1);
    assert_eq!(j.status, JobStatus::Failed);
    assert!(j.error.unwrap().contains("not retried"));
    assert!(r.api.times_of(is_send)[0] - posts[0] >= 30_000, "the next write still waits Retry-After");
    assert_eq!(job(&r.q, 2).status, JobStatus::Sent);
}

#[tokio::test]
async fn reaction_and_thread_creation_not_retried() {
    let r = rig(&[20_000]);
    r.api.fail_on("react", DiscordError::RateLimited { retry_after_s: 1 });
    r.api.fail_on("create_thread", DiscordError::RateLimited { retry_after_s: 1 });
    r.q.enqueue(JobSpec::React { channel: CH, message: Id(5), emoji: "x".into() }).unwrap();
    r.q.enqueue(JobSpec::Thread { channel: CH, from_message: None, name: "T".into(), content: Some("hi".into()) })
        .unwrap();
    drain(&r.q).await;
    let calls = r.api.calls();
    assert_eq!(calls.iter().filter(|c| matches!(c, Call::React { .. })).count(), 1);
    assert_eq!(calls.iter().filter(|c| matches!(c, Call::CreateThread { .. })).count(), 1);
    assert!(!calls.iter().any(is_send), "no thread, so no message");
    assert_eq!((job(&r.q, 1).status, job(&r.q, 2).status), (JobStatus::Failed, JobStatus::Failed));
}

#[tokio::test]
async fn forum_post_types_then_posts_with_a_nonce() {
    let r = rig(&[20_000]);
    r.q.enqueue(JobSpec::ForumPost { forum: FORUM, name: "Title".into(), content: "body".into() }).unwrap();
    drain(&r.q).await;
    let calls = r.api.calls();
    assert_eq!(calls[0], Call::Typing(FORUM));
    let Call::CreateForumPost { forum, nonce, .. } = &calls[1] else { panic!("{calls:?}") };
    assert_eq!(*forum, FORUM);
    let posted = r.api.times_of(|c| matches!(c, Call::CreateForumPost { .. }))[0];
    assert_eq!(nonce.timestamp_ms(), T0 + posted, "the nonce is made at send time");
    let j = job(&r.q, 1);
    assert_eq!(j.sent_message_id, Some(nonce.to_string()));
    assert_eq!(j.thread_id, Some(nonce.to_string()));
}

#[tokio::test]
async fn idle_without_api_and_rejects_empty_text() {
    let timer = Arc::new(FakeTimer::at(T0));
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 1 }));
    let q = SendQueue::new(
        Arc::new(Store::open_in_memory().unwrap()),
        live,
        Arc::new(Config::default()),
        timer,
        Arc::new(SeqGaps::new(&[20_000])),
    );
    assert!(matches!(q.enqueue(send("  ")), Err(Error::Invalid(_))));
    assert!(matches!(
        q.enqueue(JobSpec::Thread { channel: CH, from_message: None, name: " ".into(), content: None }),
        Err(Error::Invalid(_))
    ));
    q.enqueue(send("hi")).unwrap();
    assert_eq!(q.step().await, Step::Idle, "no REST client attached yet");
    assert!(q.status(Some(99)).is_err());
}

#[tokio::test]
async fn finished_history_is_bounded_and_text_is_dropped() {
    let r = rig(&[20_000]);
    for i in 0..(KEEP_FINISHED + 5) {
        r.q.enqueue(send(&format!("m{i}"))).unwrap();
        drain(&r.q).await;
    }
    let s = r.q.lock();
    assert_eq!(s.jobs.len(), KEEP_FINISHED);
    assert!(s.jobs.iter().all(|j| j.spec.is_none()));
}

#[test]
fn earliest_combines_gap_pause_and_cap() {
    let w: VecDeque<i64> = (0..30).map(|i| i * 1_000).collect();
    assert_eq!(earliest(100, None, 20_000, None, &VecDeque::new()), 100);
    assert_eq!(earliest(100, Some(50), 20_000, None, &VecDeque::new()), 20_050);
    assert_eq!(earliest(100, None, 20_000, Some(9_000), &VecDeque::new()), 9_000);
    assert_eq!(earliest(100, Some(29_000), 20_000, None, &w), HOUR_MS);
}

// --- Writes whose outcome is unknown (B1) ---

fn unknown_text(j: &JobView) -> bool {
    j.error.as_deref().is_some_and(|e| e.contains("may have been sent: check the channel (read_history)"))
}

#[tokio::test]
async fn network_error_on_send_retries_once_with_the_same_nonce_then_is_unknown() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::Http("timed out".into()));
    r.api.fail_on("send", DiscordError::Http("timed out".into()));
    r.q.enqueue(send("hi")).unwrap();
    drain(&r.q).await;
    let nonces = send_nonces(&r.api);
    assert_eq!(nonces.len(), 2, "one prompt retry");
    assert_eq!(nonces[0], nonces[1], "same nonce, so Discord delivers it once");
    let t = r.api.times_of(is_send);
    assert!(t[1] - t[0] < MIN_GAP_MS as i64, "promptly, not after a gap");
    let j = job(&r.q, 1);
    assert_eq!(j.status, JobStatus::Unknown);
    assert!(unknown_text(&j), "{:?}", j.error);
}

#[tokio::test]
async fn network_error_then_success_on_the_retry_is_sent() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::Api { status: 502, code: None });
    r.q.enqueue(send("hi")).unwrap();
    drain(&r.q).await;
    let j = job(&r.q, 1);
    assert_eq!(j.status, JobStatus::Sent);
    assert!(j.sent_message_id.is_some());
}

#[tokio::test]
async fn unknown_outcome_on_writes_without_nonce_is_not_retried() {
    let r = rig(&[20_000]);
    r.api.fail_on("create_forum_post", DiscordError::Api { status: 504, code: None });
    // A 2xx whose body didn't parse: the write probably happened.
    r.api.fail_on("react", DiscordError::Api { status: 200, code: None });
    r.api.fail_on("create_thread", DiscordError::Http("connection reset".into()));
    r.q.enqueue(JobSpec::ForumPost { forum: FORUM, name: "T".into(), content: "x".into() }).unwrap();
    r.q.enqueue(JobSpec::React { channel: CH, message: Id(5), emoji: "x".into() }).unwrap();
    r.q.enqueue(JobSpec::Thread { channel: CH, from_message: None, name: "T".into(), content: Some("hi".into()) })
        .unwrap();
    drain(&r.q).await;
    let calls = r.api.calls();
    let count = |f: fn(&Call) -> bool| calls.iter().filter(|c| f(c)).count();
    assert_eq!(count(|c| matches!(c, Call::CreateForumPost { .. })), 1);
    assert_eq!(count(|c| matches!(c, Call::React { .. })), 1);
    assert_eq!(count(|c| matches!(c, Call::CreateThread { .. })), 1);
    assert_eq!(count(is_send), 0);
    for id in 1..=3 {
        let j = job(&r.q, id);
        assert_eq!(j.status, JobStatus::Unknown, "job {id}");
        assert!(unknown_text(&j), "job {id}: {:?}", j.error);
    }
}

#[tokio::test]
async fn network_error_on_typing_is_a_plain_failure() {
    let r = rig(&[20_000]);
    r.api.fail_on("typing", DiscordError::Http("timed out".into()));
    r.q.enqueue(send("hi")).unwrap();
    drain(&r.q).await;
    let j = job(&r.q, 1);
    assert_eq!(j.status, JobStatus::Failed, "nothing was written");
    assert!(send_nonces(&r.api).is_empty());
}

// --- Failed attempts still pace (B2) ---

#[tokio::test]
async fn failed_attempt_still_counts_for_the_gap_and_the_cap() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::Forbidden);
    r.q.enqueue(send("one")).unwrap();
    r.q.enqueue(send("two")).unwrap();
    drain(&r.q).await;
    assert_eq!(job(&r.q, 1).status, JobStatus::Failed);
    let t = r.api.times_of(is_send);
    let typed = r.api.times_of(is_typing);
    assert!(typed[1] - t[0] >= MIN_GAP_MS as i64, "the second job waits 20-60 s after the failed attempt");
    assert_eq!(r.q.status(None).unwrap().writes_last_hour, 2);
}

// --- Resume after an edge block (a) ---

#[tokio::test]
async fn resume_after_a_block_still_waits_its_retry_after() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::Blocked { retry_after_s: 600 });
    r.q.enqueue(send("one")).unwrap();
    r.q.enqueue(send("two")).unwrap();
    drain(&r.q).await;
    let blocked_at = r.api.times_of(is_send)[0];
    r.q.resume().unwrap();
    drain(&r.q).await;
    let t = r.api.times_of(is_typing);
    assert!(t[1] >= blocked_at + 600_000, "the next write waits the block's Retry-After");
    assert_eq!(job(&r.q, 2).status, JobStatus::Sent);
}

// --- Pacing survives a restart (b) ---

#[tokio::test]
async fn pacing_survives_a_restart() {
    let r = rig(&[30_000]);
    r.q.enqueue(send("before")).unwrap();
    drain(&r.q).await;
    let first = r.api.times_of(is_send)[0];
    // A new queue on the same store, as after `pm2 restart`.
    let q2 = SendQueue::new(
        r.store.clone(),
        r.live.clone(),
        Arc::new(Config::default()),
        r.api.timer.clone(),
        Arc::new(SeqGaps::new(&[30_000])),
    );
    q2.attach(r.api.clone());
    let e = q2.enqueue(send("after")).unwrap();
    assert!(e.eta_secs >= 20, "eta counts from the last write before the restart");
    drain(&q2).await;
    let t = r.api.times_of(is_typing);
    assert!(t[1] - first >= 30_000);
    assert_eq!(q2.status(None).unwrap().writes_last_hour, 2, "the hourly window too");
}

// --- Messages that arrived while the job waited (c) ---

#[tokio::test]
async fn messages_that_arrived_while_waiting_are_listed_as_unseen() {
    let r = rig(&[20_000]);
    let read = crate::test_support::id_at(T0 - 60_000);
    r.live.apply(GatewayEvent::MessageAck { channel: CH, message: read, mention_count: None }, T0);
    r.q.enqueue(send("my reply")).unwrap();
    // Someone posts after the user approved the reply, before it goes out.
    let mut late = crate::test_support::msg(crate::test_support::id_at(T0 + 1_000), CH.0, 2, "nvm, solved it");
    late.guild_id = Some(Id(10));
    late.attachments = vec!["image: a.png".into()];
    let mut old = crate::test_support::msg(crate::test_support::id_at(T0 - 120_000), CH.0, 2, "seen already");
    old.guild_id = Some(Id(10));
    r.api.inner.put_history(CH, vec![old.clone(), late.clone()]);
    r.live.apply(GatewayEvent::MessageCreate(Box::new(late.clone())), T0 + 1_000);
    drain(&r.q).await;
    let j = job(&r.q, 1);
    assert_eq!(j.status, JobStatus::Sent);
    let unseen = j.unseen.expect("unseen listed");
    assert_eq!(unseen.len(), 1, "only others' messages after the read point, before the send");
    assert_eq!(unseen[0].id, late.id.to_string());
    assert_eq!(unseen[0].from, "U002");
    assert_eq!(unseen[0].text, "nvm, solved it [image: a.png]");
}

#[tokio::test]
async fn nothing_new_means_no_history_call() {
    let r = rig(&[20_000]);
    let read = crate::test_support::id_at(T0 - 60_000);
    r.live.apply(GatewayEvent::MessageAck { channel: CH, message: read, mention_count: None }, T0);
    r.q.enqueue(send("hi")).unwrap();
    drain(&r.q).await;
    assert!(!r.api.calls().iter().any(|c| matches!(c, Call::History { .. })));
    assert_eq!(job(&r.q, 1).unseen, None);
}

// --- Resume also lifts the REST client's halt (d) ---

#[tokio::test]
async fn resume_clears_the_rest_clients_halt_too() {
    let r = rig(&[20_000]);
    r.api.fail_on("send", DiscordError::Captcha);
    r.q.enqueue(send("one")).unwrap();
    drain(&r.q).await;
    assert_eq!(r.api.cleared(), 0);
    r.q.resume().unwrap();
    assert_eq!(r.api.cleared(), 1);
}
