//! Every write goes through an in-memory queue, so tritone never writes in
//! bursts: one job at a time, the first at once, each later one a random
//! 20–60 s after the previous write, at most 30 per rolling hour. Messages
//! (sends, thread messages, forum posts) show the typing indicator for 2–5 s
//! first; reactions skip typing but keep the gaps.
//!
//! Text lives only in memory and is dropped when the job finishes; a restart
//! drops unsent jobs; the pacing itself (last write, last hour) is kept in
//! `kv`, so a restart can't burst. Every attempt counts toward the pacing,
//! failed or not.
//!
//! Retries are limited to what Discord can deduplicate, a message with a
//! nonce: once after a rate limit (after its Retry-After), and once promptly
//! when a send's outcome is unknown (network error, 5xx, unreadable 2xx).
//! Writes without a nonce (thread creation, forum posts, reactions) are never
//! retried. A write whose outcome stays unknown is reported as `unknown`,
//! never as `failed`, so it isn't approved and sent twice. A halting error (CAPTCHA, locked account, edge block, dead
//! token) fails that job, pauses the queue with the other jobs still queued,
//! and halts all of tritone through `LiveState`. Only `resume` (the user's
//! explicit "continue") lifts it.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use super::activity::{channel_label, message_link, record_own};
use super::clock::{GapRng, Timer};
use super::errors::{Error, Result};
use super::live::{ConnState, LiveState};
use crate::config::Config;
use crate::discord::DiscordApi;
use crate::discord::types::{DiscordError, Id, Message, OutgoingMessage};
use crate::store::Store;

pub const MIN_GAP_MS: u64 = 20_000;
pub const MAX_GAP_MS: u64 = 60_000;
pub const HOURLY_CAP: usize = 30;
pub const TYPING_MIN_MS: u64 = 2_000;
pub const TYPING_MAX_MS: u64 = 5_000;
const HOUR_MS: i64 = 3_600_000;
/// Never pause longer than this for one rate limit.
pub const RATE_LIMIT_MAX_SECS: u64 = 600;
/// Finished jobs kept for send_status.
const KEEP_FINISHED: usize = 50;
/// How often a queue waiting for the connection checks again.
const NOT_READY_POLL_MS: i64 = 5_000;
/// Wait before re-sending a message whose outcome is unknown. Short, because
/// Discord only deduplicates a nonce "in a short period of time"
/// (https://docs.discord.food/resources/message#create-message).
const UNKNOWN_RETRY_MS: i64 = 2_000;
/// Others' messages listed per job as unseen.
const MAX_UNSEEN: usize = 20;
/// `kv` key of the pacing state.
const KV_PACING: &str = "send_pacing";
/// What an `unknown` job tells Claude.
const MAY_HAVE_BEEN_SENT: &str = "may have been sent: check the channel (read_history) before queueing it again";

/// One write. Text fields are dropped once the job finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobSpec {
    /// `kind` labels the contribution log entry (an opportunity rule, or one
    /// Claude chose); `None` means inferred.
    Send {
        channel: Id,
        content: String,
        reply_to: Option<Id>,
        kind: Option<String>,
    },
    React {
        channel: Id,
        message: Id,
        emoji: String,
    },
    /// A thread, from a message or standalone, with an optional first message.
    Thread {
        channel: Id,
        from_message: Option<Id>,
        name: String,
        content: Option<String>,
    },
    ForumPost {
        forum: Id,
        name: String,
        content: String,
    },
}

impl JobSpec {
    fn target(&self) -> Id {
        match self {
            JobSpec::Send { channel, .. } | JobSpec::React { channel, .. } | JobSpec::Thread { channel, .. } => {
                *channel
            }
            JobSpec::ForumPost { forum, .. } => *forum,
        }
    }

    fn validate(&self) -> Result<()> {
        let blank = |s: &str| s.trim().is_empty();
        let bad = match self {
            JobSpec::Send { content, .. } => blank(content).then_some("text is empty"),
            JobSpec::React { emoji, .. } => blank(emoji).then_some("emoji is empty"),
            JobSpec::Thread { name, content, .. } => {
                if blank(name) {
                    Some("thread name is empty")
                } else {
                    content.as_deref().is_some_and(blank).then_some("text is empty")
                }
            }
            JobSpec::ForumPost { name, content, .. } => {
                if blank(name) {
                    Some("title is empty")
                } else {
                    blank(content).then_some("text is empty")
                }
            }
        };
        bad.map_or(Ok(()), |m| Err(Error::Invalid(m.into())))
    }

    fn what(&self) -> &'static str {
        match self {
            JobSpec::Send { .. } => "message",
            JobSpec::React { .. } => "reaction",
            JobSpec::Thread { .. } => "thread",
            JobSpec::ForumPost { .. } => "forum_post",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Sending,
    Sent,
    Failed,
    /// The write may or may not have happened (network error, 5xx, unreadable
    /// reply). Check the channel before queueing it again.
    Unknown,
}

/// A message someone else posted while the job waited: between the channel's
/// read point at enqueue and the sent message. The ack after the send marks
/// it read, so `send_status` shows it instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnseenMessage {
    pub id: String,
    pub from: String,
    pub from_id: String,
    pub at: String,
    /// The text, with attachments and embeds as `[image: …]`, `[embed: …]`.
    pub text: String,
}

#[derive(Debug, Clone)]
struct Job {
    id: u64,
    /// `None` once finished: text never outlives its write.
    spec: Option<JobSpec>,
    what: &'static str,
    target: Id,
    guild: Option<Id>,
    label: String,
    /// Wait after the previous write, drawn at enqueue time so the ETA is exact.
    gap_ms: u64,
    status: JobStatus,
    /// Made on the first attempt and reused by the retry.
    nonce: Option<Id>,
    /// A thread or forum post this job created (kept across a retry).
    thread: Option<Id>,
    sent_message: Option<Id>,
    sent_at_ms: Option<i64>,
    error: Option<String>,
    ack_error: Option<String>,
    retried: bool,
    enqueued_ms: i64,
    /// Sends only: the channel's last acked message at enqueue.
    read_point: Option<Id>,
    /// Sends only: everything after this, up to the sent message, counts as
    /// unseen (set just before the send), and the channel's newest message then.
    unseen_after: Option<Id>,
    newest_before_send: Option<Id>,
    unseen: Option<Vec<UnseenMessage>>,
    unseen_more: bool,
    unseen_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JobView {
    pub job_id: u64,
    /// message, reaction, thread or forum_post.
    pub what: &'static str,
    pub target: String,
    pub target_id: String,
    pub status: JobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<i64>,
    /// The message WAS sent: never send it again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Sent, but marking the channel read failed. Never resend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ack_error: Option<String>,
    /// Others' messages that arrived while the job waited, oldest first,
    /// marked read by the send: show them to the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unseen: Option<Vec<UnseenMessage>>,
    /// More unseen messages than listed: read the channel.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unseen_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unseen_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Enqueued {
    pub job_id: u64,
    /// 1 = next to go.
    pub position: usize,
    pub eta_secs: u64,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueStatus {
    pub jobs: Vec<JobView>,
    /// Why the queue is stopped; `resume_queue` continues after the user's OK.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<String>,
    pub writes_last_hour: usize,
    pub hourly_cap: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing to do: empty, paused, or no REST client yet.
    Idle,
    /// Jobs are waiting for the connection to be ready.
    NotReady,
    /// Slept until the next job's slot.
    Waited,
    /// One attempt finished.
    Processed,
}

#[derive(Default)]
struct State {
    jobs: VecDeque<Job>,
    next_id: u64,
    last_write_ms: Option<i64>,
    /// Times of recent writes, at most `HOURLY_CAP` of them.
    window: VecDeque<i64>,
    /// Set by a rate limit: no write before then.
    paused_until_ms: Option<i64>,
    /// Set by a halting error; only `resume` clears it.
    paused: Option<DiscordError>,
    /// When `paused` was set: an edge block's Retry-After counts from then.
    paused_at_ms: Option<i64>,
    api: Option<Arc<dyn DiscordApi>>,
}

/// The pacing state kept in `kv` across restarts.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Pacing {
    last_write_ms: Option<i64>,
    window: Vec<i64>,
}

fn load_pacing(store: &Store) -> Pacing {
    match store.kv_get(KV_PACING) {
        Ok(Some(v)) => serde_json::from_str(&v).unwrap_or_default(),
        Ok(None) => Pacing::default(),
        Err(e) => {
            tracing::warn!("reading the send pacing failed: {e}");
            Pacing::default()
        }
    }
}

/// The write may have happened: the connection failed mid-request, Discord's
/// edge returned a 5xx, or a 2xx came back that couldn't be read.
fn outcome_unknown(e: &DiscordError) -> bool {
    match e {
        DiscordError::Http(_) => true,
        DiscordError::Api { status, .. } => *status >= 500 || (200..300).contains(status),
        _ => false,
    }
}

/// Why an attempt failed, and whether the write may have gone out anyway.
struct Failure {
    e: DiscordError,
    maybe_sent: bool,
}

impl Failure {
    /// A failure before anything was written (e.g. typing).
    fn before_write(e: DiscordError) -> Failure {
        Failure { e, maybe_sent: false }
    }

    /// A failure of the write call itself.
    fn of_write(e: DiscordError) -> Failure {
        let maybe_sent = outcome_unknown(&e);
        Failure { e, maybe_sent }
    }
}

/// One line of text for an unseen message: the text plus placeholders.
fn unseen_text(m: &Message) -> String {
    let mut parts = vec![m.content.clone()];
    parts.extend(m.attachments.iter().map(|a| format!("[{a}]")));
    parts.extend(m.embeds.iter().map(|t| format!("[embed: {t}]")));
    parts.retain(|p| !p.is_empty());
    parts.join(" ")
}

/// Halts all of tritone after a halting error, unless it's already halted:
/// the REST client may have rejected the token (and remembered it), and a
/// plain halt would forget which token died.
pub(crate) fn halt_everything(live: &LiveState, e: &DiscordError) {
    if !matches!(live.connection(), ConnState::Halted(_)) {
        live.halt(e.clone());
    }
}

/// When a job with `gap_ms` may start.
fn earliest(now: i64, last: Option<i64>, gap_ms: u64, paused_until: Option<i64>, window: &VecDeque<i64>) -> i64 {
    let mut t = now;
    if let Some(l) = last {
        t = t.max(l + gap_ms as i64);
    }
    if let Some(p) = paused_until {
        t = t.max(p);
    }
    if window.len() >= HOURLY_CAP {
        t = t.max(window[window.len() - HOURLY_CAP] + HOUR_MS);
    }
    t
}

fn push_window(window: &mut VecDeque<i64>, at: i64) {
    window.push_back(at);
    while window.len() > HOURLY_CAP {
        window.pop_front();
    }
}

/// Start time of every queued job, simulating the queue in order.
fn etas(s: &State, now: i64) -> HashMap<u64, i64> {
    let mut last = s.last_write_ms;
    let mut window = s.window.clone();
    // A write in flight lands about now.
    if s.jobs.iter().any(|j| j.status == JobStatus::Sending) {
        last = Some(now);
        push_window(&mut window, now);
    }
    let mut out = HashMap::new();
    for j in s.jobs.iter().filter(|j| j.status == JobStatus::Queued) {
        let at = earliest(now, last, j.gap_ms, s.paused_until_ms, &window);
        out.insert(j.id, at);
        last = Some(at);
        push_window(&mut window, at);
    }
    out
}

fn rfc3339_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| ms.to_string())
}

fn view(j: &Job, eta: Option<i64>, now: i64) -> JobView {
    let link = match (j.sent_message, j.thread) {
        (Some(m), _) => Some(message_link(j.guild, j.thread.unwrap_or(j.target), m)),
        (None, Some(t)) => {
            Some(format!("https://discord.com/channels/{}/{t}", j.guild.map_or("@me".into(), |g| g.to_string())))
        }
        (None, None) => None,
    };
    JobView {
        job_id: j.id,
        what: j.what,
        target: j.label.clone(),
        target_id: j.target.to_string(),
        status: j.status,
        eta_secs: eta.map(|e| (e - now).max(0).div_euclid(1000)),
        sent_message_id: j.sent_message.map(|m| m.to_string()),
        thread_id: j.thread.map(|t| t.to_string()),
        link,
        sent_at: j.sent_at_ms.map(rfc3339_ms),
        error: j.error.clone(),
        ack_error: j.ack_error.clone(),
        unseen: j.unseen.clone(),
        unseen_more: j.unseen_more,
        unseen_error: j.unseen_error.clone(),
    }
}

/// Whether a retry can only re-send a nonce'd message, which Discord
/// delivers once: a Send, or a thread whose creation already succeeded (only
/// its first message is left). Thread creation, forum posts and reactions
/// carry no nonce, so retrying them could duplicate the write.
fn nonce_protected(j: &Job) -> bool {
    match j.what {
        "message" => true,
        "thread" => j.thread.is_some(),
        _ => false,
    }
}

fn fail(j: &mut Job, reason: String) {
    j.status = JobStatus::Failed;
    j.error = Some(reason);
    j.spec = None;
}

/// The write may have happened: never shown as failed.
fn mark_unknown(j: &mut Job, e: &DiscordError) {
    let what = match j.thread {
        Some(t) if j.what == "thread" => format!("the thread was created (id {t}); its first message "),
        _ => String::new(),
    };
    j.status = JobStatus::Unknown;
    j.error = Some(format!("{what}{MAY_HAVE_BEEN_SENT} ({e})"));
    j.spec = None;
}

fn trim_finished(jobs: &mut VecDeque<Job>) {
    let finished = |j: &Job| matches!(j.status, JobStatus::Sent | JobStatus::Failed | JobStatus::Unknown);
    let mut extra = jobs.iter().filter(|j| finished(j)).count().saturating_sub(KEEP_FINISHED);
    jobs.retain(|j| {
        if extra > 0 && finished(j) {
            extra -= 1;
            return false;
        }
        true
    });
}

pub struct SendQueue {
    state: Mutex<State>,
    notify: Notify,
    store: Arc<Store>,
    live: Arc<LiveState>,
    config: Arc<Config>,
    timer: Arc<dyn Timer>,
    rng: Arc<dyn GapRng>,
    nonce_counter: AtomicU16,
}

impl SendQueue {
    pub fn new(
        store: Arc<Store>,
        live: Arc<LiveState>,
        config: Arc<Config>,
        timer: Arc<dyn Timer>,
        rng: Arc<dyn GapRng>,
    ) -> SendQueue {
        let pacing = load_pacing(&store);
        SendQueue {
            state: Mutex::new(State {
                next_id: 1,
                last_write_ms: pacing.last_write_ms,
                window: pacing.window.into(),
                ..State::default()
            }),
            notify: Notify::new(),
            store,
            live,
            config,
            timer,
            rng,
            nonce_counter: AtomicU16::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The REST client to write with. The write tools attach the current one
    /// on every enqueue, so a client rebuilt after a new login is picked up.
    /// Attaching never lifts a pause.
    pub fn attach(&self, api: Arc<dyn DiscordApi>) {
        self.lock().api = Some(api);
        self.notify.notify_one();
    }

    pub fn enqueue(&self, spec: JobSpec) -> Result<Enqueued> {
        spec.validate()?;
        let now = self.timer.now_ms();
        let target = spec.target();
        let guild = self.live.channel(target).and_then(|c| c.guild_id);
        let label = channel_label(&self.live, target);
        let read_point = match &spec {
            JobSpec::Send { .. } => self.live.read_state(target).last_acked,
            _ => None,
        };
        let mut s = self.lock();
        let id = s.next_id;
        s.next_id += 1;
        s.jobs.push_back(Job {
            id,
            what: spec.what(),
            spec: Some(spec),
            target,
            guild,
            label,
            gap_ms: self.rng.gap_ms(MIN_GAP_MS, MAX_GAP_MS),
            status: JobStatus::Queued,
            nonce: None,
            thread: None,
            sent_message: None,
            sent_at_ms: None,
            error: None,
            ack_error: None,
            retried: false,
            enqueued_ms: now,
            read_point,
            unseen_after: None,
            newest_before_send: None,
            unseen: None,
            unseen_more: false,
            unseen_error: None,
        });
        let eta = etas(&s, now).get(&id).copied().unwrap_or(now);
        let position = s.jobs.iter().filter(|j| j.status == JobStatus::Queued).count();
        let note = match &s.paused {
            Some(e) => format!("Queued, but the queue is paused: {e}"),
            None => {
                "Queued, not sent yet. Writes go out 20-60 s apart, messages after 2-5 s of typing; check send_status."
                    .into()
            }
        };
        drop(s);
        self.notify.notify_one();
        Ok(Enqueued { job_id: id, position, eta_secs: (eta - now).max(0).div_euclid(1000) as u64, note })
    }

    /// One job (`Some`) or all recent jobs, oldest first.
    pub fn status(&self, job_id: Option<u64>) -> Result<QueueStatus> {
        let now = self.timer.now_ms();
        let s = self.lock();
        let etas = etas(&s, now);
        let jobs: Vec<JobView> = s
            .jobs
            .iter()
            .filter(|j| job_id.is_none_or(|id| j.id == id))
            .map(|j| view(j, etas.get(&j.id).copied(), now))
            .collect();
        if let Some(id) = job_id
            && jobs.is_empty()
        {
            return Err(Error::Invalid(format!("no job {id}: unsent jobs are dropped when tritone restarts")));
        }
        Ok(QueueStatus {
            jobs,
            paused: s.paused.as_ref().map(|e| e.to_string()),
            writes_last_hour: s.window.iter().filter(|t| now - **t < HOUR_MS).count(),
            hourly_cap: HOURLY_CAP,
        })
    }

    /// The user's explicit "continue" after a halt. Refuses while the login
    /// is dead (`tritone login` fixes that) or after the gateway closed or
    /// kept refusing the session (only a restart fixes that). For a CAPTCHA, a locked account
    /// or an edge block it lifts tritone's halt and the REST client's own,
    /// then resumes the queue once the connection is ready; after a block,
    /// writes still wait its Retry-After, counted from when it hit. The job
    /// that hit the error stays failed.
    pub fn resume(&self) -> Result<QueueStatus> {
        let live_halt = match self.live.connection() {
            // None can be continued: a dead login needs `tritone login`; a
            // gateway that closed or keeps refusing the session needs a restart.
            ConnState::Halted(
                e @ (DiscordError::NeedsLogin | DiscordError::GatewayClosed { .. } | DiscordError::SessionRefused),
            ) => {
                return Err(Error::Discord(e));
            }
            ConnState::Halted(e) => Some(e),
            _ => None,
        };
        let (api, block_s, paused_at) = {
            let s = self.lock();
            let block = |e: Option<&DiscordError>| match e {
                Some(DiscordError::Blocked { retry_after_s }) => Some(*retry_after_s),
                _ => None,
            };
            let block_s = block(live_halt.as_ref()).max(block(s.paused.as_ref()));
            (s.api.clone(), block_s, s.paused_at_ms)
        };
        self.live.clear_halt();
        if let Some(api) = api {
            api.clear_halt();
        }
        if !matches!(self.live.connection(), ConnState::Ready { .. }) {
            return Err(Error::Invalid(
                "tritone is not connected to Discord right now. Call status, and resume_queue again once it's ready."
                    .into(),
            ));
        }
        let now = self.timer.now_ms();
        let mut s = self.lock();
        if let Some(secs) = block_s {
            let until = paused_at.unwrap_or(now) + secs as i64 * 1000;
            s.paused_until_ms = s.paused_until_ms.max(Some(until));
        }
        s.paused = None;
        s.paused_at_ms = None;
        drop(s);
        tracing::info!("send queue resumed");
        self.notify.notify_one();
        self.status(None)
    }

    /// The worker: runs forever, one step at a time.
    pub async fn run(self: Arc<Self>) {
        loop {
            let notified = self.notify.notified();
            match self.step().await {
                Step::Idle => notified.await,
                Step::NotReady => {
                    let poll = self.timer.sleep_until(self.timer.now_ms() + NOT_READY_POLL_MS);
                    tokio::select! {
                        _ = notified => {}
                        _ = poll => {}
                    }
                }
                Step::Waited | Step::Processed => {}
            }
        }
    }

    /// Waits for or performs the next job.
    pub async fn step(&self) -> Step {
        let Some((id, start, api)) = self.next_job() else { return Step::Idle };
        match self.live.connection() {
            ConnState::Ready { .. } => {}
            ConnState::Halted(e) => {
                // Halted elsewhere (gateway, another tool): stop here too.
                self.pause(e);
                return Step::Idle;
            }
            _ => return Step::NotReady,
        }
        if start > self.timer.now_ms() {
            self.timer.sleep_until(start).await;
            return Step::Waited;
        }
        let Some(job) = self.begin(id) else { return Step::Idle };
        // Ok carries the posted message, if the write posted one.
        match self.perform(&job, api.as_ref()).await {
            Ok(message) => self.on_done(&job, api.as_ref(), message).await,
            Err(e) => self.on_error(id, e),
        }
        Step::Processed
    }

    fn next_job(&self) -> Option<(u64, i64, Arc<dyn DiscordApi>)> {
        let now = self.timer.now_ms();
        let s = self.lock();
        if s.paused.is_some() {
            return None;
        }
        let api = s.api.clone()?;
        let j = s.jobs.iter().find(|j| j.status == JobStatus::Queued)?;
        Some((j.id, earliest(now, s.last_write_ms, j.gap_ms, s.paused_until_ms, &s.window), api))
    }

    /// Marks the job as sending and returns a snapshot of it.
    fn begin(&self, id: u64) -> Option<Job> {
        let mut s = self.lock();
        let j = s.jobs.iter_mut().find(|j| j.id == id)?;
        j.status = JobStatus::Sending;
        Some(j.clone())
    }

    /// The job's nonce: made now (a snowflake of the send time, like the
    /// client's) on the first attempt, the same one on the retry.
    fn nonce_for(&self, id: u64) -> Id {
        let now = self.timer.now_ms();
        let counter = self.nonce_counter.fetch_add(1, Ordering::Relaxed) & 0x0fff;
        let mut s = self.lock();
        match s.jobs.iter_mut().find(|j| j.id == id) {
            Some(j) => *j.nonce.get_or_insert(Id::from_timestamp_ms(now, counter)),
            None => Id::from_timestamp_ms(now, counter),
        }
    }

    /// Shows typing for 2–5 s, like a person composing.
    async fn type_for_a_while(&self, api: &dyn DiscordApi, channel: Id) -> std::result::Result<(), DiscordError> {
        api.typing(channel).await?;
        let wait = self.rng.gap_ms(TYPING_MIN_MS, TYPING_MAX_MS);
        self.timer.sleep_until(self.timer.now_ms() + wait as i64).await;
        Ok(())
    }

    /// Sends a nonce'd message. When the outcome is unknown, sends it again
    /// once, promptly, with the same nonce: Discord delivers it only once. A
    /// failure after an unknown first attempt stays "maybe sent".
    async fn send_nonced(
        &self,
        api: &dyn DiscordApi,
        channel: Id,
        msg: &OutgoingMessage,
    ) -> std::result::Result<Message, Failure> {
        match api.send(channel, msg).await {
            Ok(m) => Ok(m),
            Err(e) if outcome_unknown(&e) => {
                tracing::warn!(kind = e.kind(), "send outcome unknown; sending again with the same nonce");
                self.timer.sleep_until(self.timer.now_ms() + UNKNOWN_RETRY_MS).await;
                api.send(channel, msg).await.map_err(|e| Failure { e, maybe_sent: true })
            }
            Err(e) => Err(Failure::of_write(e)),
        }
    }

    /// Remembers, just before a send, where "unseen" starts: the read point
    /// at enqueue or a later one (the user may have read since), else the
    /// enqueue time. Taken before the send because the send's own echo acks
    /// the channel.
    fn mark_unseen_window(&self, job: &Job, channel: Id) {
        let acked_now = self.live.read_state(channel).last_acked;
        let after = job.read_point.max(acked_now).unwrap_or_else(|| Id::from_timestamp_ms(job.enqueued_ms, 0));
        let newest = self.live.channel(channel).and_then(|c| c.last_message_id);
        self.update(job.id, |_, j| (j.unseen_after, j.newest_before_send) = (Some(after), newest));
    }

    async fn perform(&self, job: &Job, api: &dyn DiscordApi) -> std::result::Result<Option<Message>, Failure> {
        let Some(spec) = &job.spec else { return Ok(None) };
        let out = |content: &str, reply_to: Option<Id>| OutgoingMessage {
            content: content.to_string(),
            reply_to,
            nonce: self.nonce_for(job.id),
            silent: false,
        };
        match spec {
            JobSpec::Send { channel, content, reply_to, .. } => {
                self.type_for_a_while(api, *channel).await.map_err(Failure::before_write)?;
                self.mark_unseen_window(job, *channel);
                self.send_nonced(api, *channel, &out(content, *reply_to)).await.map(Some)
            }
            JobSpec::React { channel, message, emoji } => {
                api.react(*channel, *message, emoji).await.map(|()| None).map_err(Failure::of_write)
            }
            JobSpec::Thread { channel, from_message, name, content } => {
                let thread = match job.thread {
                    Some(t) => t,
                    None => {
                        let t = api.create_thread(*channel, *from_message, name).await.map_err(Failure::of_write)?.id;
                        self.update(job.id, |_, j| j.thread = Some(t));
                        t
                    }
                };
                let Some(text) = content else { return Ok(None) };
                self.type_for_a_while(api, thread).await.map_err(Failure::before_write)?;
                self.send_nonced(api, thread, &out(text, None)).await.map(Some)
            }
            JobSpec::ForumPost { forum, name, content } => {
                self.type_for_a_while(api, *forum).await.map_err(Failure::before_write)?;
                let (thread, msg) = api
                    .create_forum_post(*forum, name, content, self.nonce_for(job.id))
                    .await
                    .map_err(Failure::of_write)?;
                self.update(job.id, |_, j| j.thread = Some(thread.id));
                Ok(Some(msg))
            }
        }
    }

    /// Others' messages posted while a send waited, so `send_status` can show
    /// them before the ack marks them read. Reads history only when the
    /// channel had something newer than the read point.
    async fn check_unseen(&self, id: u64, api: &dyn DiscordApi, sent: &Message) {
        let (after, newest) = {
            let s = self.lock();
            match s.jobs.iter().find(|j| j.id == id) {
                Some(j) => (j.unseen_after, j.newest_before_send),
                None => return,
            }
        };
        let Some(after) = after else { return };
        if newest.is_none_or(|n| n <= after) {
            return;
        }
        let me = self.live.me().map(|u| u.id);
        match api.history(sent.channel_id, None, Some(after), 50).await {
            Ok(page) => {
                let mut others: Vec<&Message> =
                    page.iter().filter(|m| m.id > after && m.id < sent.id && Some(m.author.id) != me).collect();
                others.sort_by_key(|m| m.id);
                let more = others.len() > MAX_UNSEEN;
                let unseen: Vec<UnseenMessage> = others
                    .into_iter()
                    .take(MAX_UNSEEN)
                    .map(|m| UnseenMessage {
                        id: m.id.to_string(),
                        from: m.author.label().to_string(),
                        from_id: m.author.id.to_string(),
                        at: rfc3339_ms(m.timestamp_ms),
                        text: unseen_text(m),
                    })
                    .collect();
                self.update(id, |_, j| {
                    j.unseen_more = more;
                    j.unseen = (!unseen.is_empty()).then_some(unseen);
                });
            }
            Err(e) => {
                tracing::warn!(job = id, kind = e.kind(), "checking for unseen messages failed");
                let note = format!("could not check for messages that arrived meanwhile: {e}. Read the channel.");
                self.update(id, |_, j| j.unseen_error = Some(note));
                if e.halts() {
                    self.halt(e);
                }
            }
        }
    }

    async fn on_done(&self, job: &Job, api: &dyn DiscordApi, message: Option<Message>) {
        let now = self.timer.now_ms();
        let kind = match &job.spec {
            Some(JobSpec::Send { kind, .. }) => kind.clone(),
            _ => None,
        };
        self.note_write(now);
        self.update(job.id, |_, j| {
            j.status = JobStatus::Sent;
            j.sent_at_ms = Some(now);
            j.sent_message = message.as_ref().map(|m| m.id);
            j.spec = None;
        });
        tracing::info!(job = job.id, what = job.what, "write done");
        let Some(msg) = message else { return };
        if let Err(e) = record_own(&self.store, &self.live, &self.config.topics, &msg, job.guild, kind.as_deref(), true)
        {
            // Only the contribution log depends on this; the message is out.
            tracing::warn!("recording a sent message failed: {e}");
        }
        // A reply answers the opportunity it replies to, if there is one.
        if let Some(JobSpec::Send { reply_to: Some(r), .. }) = &job.spec
            && let Err(e) = self.store.opportunity_answered(*r, now)
        {
            tracing::warn!("marking an opportunity answered failed: {e}");
        }
        // From here on the message is out: a failure must not look like a
        // failed send, or it gets approved and sent again.
        if matches!(job.spec, Some(JobSpec::Send { .. })) {
            self.check_unseen(job.id, api, &msg).await;
        }
        match api.ack(msg.channel_id, msg.id).await {
            Ok(()) => self.live.note_acked(msg.channel_id, msg.id),
            Err(e) => {
                tracing::warn!(job = job.id, kind = e.kind(), "ack after send failed");
                self.update(job.id, |_, j| j.ack_error = Some(e.to_string()));
                if e.halts() {
                    self.halt(e);
                }
            }
        }
    }

    fn on_error(&self, id: u64, f: Failure) {
        let Failure { e, maybe_sent } = f;
        tracing::warn!(job = id, kind = e.kind(), maybe_sent, "write failed");
        let now = self.timer.now_ms();
        // Whatever happened, the attempt reached Discord: it paces like a write.
        self.note_write(now);
        if e.halts() {
            self.update(id, |_, j| if maybe_sent { mark_unknown(j, &e) } else { fail(j, e.to_string()) });
            self.halt(e);
            return;
        }
        self.update(id, |s, j| match &e {
            _ if maybe_sent => mark_unknown(j, &e),
            DiscordError::RateLimited { retry_after_s } => {
                // Every later write waits out the Retry-After too.
                s.paused_until_ms = Some(now + (*retry_after_s).min(RATE_LIMIT_MAX_SECS) as i64 * 1000);
                if j.retried {
                    fail(j, format!("{e} (twice; not retried again)"));
                } else if !nonce_protected(j) {
                    fail(j, format!("{e} (not retried: Discord can't deduplicate this kind of write)"));
                } else {
                    j.retried = true;
                    j.status = JobStatus::Queued;
                }
            }
            other => {
                let reason = match j.thread {
                    Some(t) if j.what == "thread" => {
                        format!("the thread was created (id {t}) but its message failed: {other}")
                    }
                    _ => other.to_string(),
                };
                fail(j, reason);
            }
        });
    }

    /// Records a write attempt for the gap and the hourly cap, and keeps the
    /// pacing in `kv` so a restart can't burst.
    fn note_write(&self, at: i64) {
        let pacing = {
            let mut s = self.lock();
            s.last_write_ms = Some(at);
            push_window(&mut s.window, at);
            Pacing { last_write_ms: s.last_write_ms, window: s.window.iter().copied().collect() }
        };
        let saved =
            serde_json::to_string(&pacing).map_err(anyhow::Error::from).and_then(|v| self.store.kv_set(KV_PACING, &v));
        if let Err(e) = saved {
            tracing::warn!("saving the send pacing failed: {e}");
        }
    }

    /// Stops the queue and all of tritone. Jobs still queued stay queued
    /// for the user to resume or abandon.
    fn halt(&self, e: DiscordError) {
        halt_everything(&self.live, &e);
        self.pause(e);
    }

    fn pause(&self, e: DiscordError) {
        let mut s = self.lock();
        if s.paused.is_none() {
            let queued = s.jobs.iter().filter(|j| j.status == JobStatus::Queued).count();
            tracing::warn!(kind = e.kind(), queued, "send queue paused");
            s.paused = Some(e);
            s.paused_at_ms = Some(self.timer.now_ms());
        }
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut State, &mut Job)) {
        let mut s = self.lock();
        let Some(pos) = s.jobs.iter().position(|j| j.id == id) else { return };
        let mut job = s.jobs[pos].clone();
        f(&mut s, &mut job);
        s.jobs[pos] = job;
        trim_finished(&mut s.jobs);
    }
}

#[cfg(test)]
#[path = "send_queue_tests.rs"]
mod tests;
