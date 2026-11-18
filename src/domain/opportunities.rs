//! Opportunities: places in full-watch server channels where the user could
//! contribute. The scanner flags candidates by rule as messages arrive and
//! keeps their ids (never text) in the store; `get_opportunities` re-checks
//! them against the live buffer, or Discord after a restart, and drops the
//! ones someone already answered.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result as AnyResult;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::config::{Config, Topic};
use crate::discord::types::{Channel, DiscordError, Id, Message};
use crate::domain::activity::topic_for;
use crate::domain::errors::{Error, Result};
use crate::domain::live::{LiveEvent, LiveState};
use crate::domain::pending::explicit_watch;
use crate::domain::render::{RenderedMessage, render_message};
use crate::domain::resolve::channel_label;
use crate::domain::send_queue::halt_everything;
use crate::mcp::state::Ctx;
use crate::store::{OpportunityRow, OpportunityStatus, OwnMessageRow, Store, WatchLevel, WatchMap};

/// How a message opens when it's probably a question (case-insensitive, whole words).
const QUESTION_OPENERS: &[&str] = &["how", "why", "what", "anyone", "does", "is there", "can i", "can someone", "help"];
/// Messages shown before an opportunity.
pub const CONTEXT_BEFORE: usize = 5;
/// `around` page size when the buffer doesn't hold the message (e.g. after a restart).
const AROUND_LIMIT: u8 = 10;
/// REST lookups per `get_opportunities` call; the rest wait for the next call.
const MAX_FETCHES: usize = 10;
/// How long the scanner trusts its copy of the watch levels.
const WATCH_TTL: Duration = Duration::from_secs(10);
const MINUTE_MS: i64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// Shaped like a question, from someone else.
    Question,
    /// Matches a configured topic; holds the topic's kind.
    Topic(String),
    /// In a thread the user wrote in, or a reply to one of their messages.
    MyThread,
}

impl Rule {
    pub fn to_db(&self) -> String {
        match self {
            Rule::Question => "question".into(),
            Rule::Topic(kind) => format!("topic:{kind}"),
            Rule::MyThread => "my_thread".into(),
        }
    }

    pub fn from_db(s: &str) -> Option<Rule> {
        match s {
            "question" => Some(Rule::Question),
            "my_thread" => Some(Rule::MyThread),
            _ => s.strip_prefix("topic:").map(|k| Rule::Topic(k.to_string())),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Rule::Question => "question",
            Rule::Topic(_) => "topic",
            Rule::MyThread => "my_thread",
        }
    }

    /// The contribution kind a reply to it is logged as (send_message's `kind`).
    pub fn kind(&self) -> String {
        match self {
            Rule::Question => "answer".into(),
            Rule::Topic(kind) => kind.clone(),
            Rule::MyThread => "thread".into(),
        }
    }
}

/// A `?` (outside links), or an opening like "how", "is there" or "can someone".
pub fn is_question(text: &str) -> bool {
    let words: Vec<String> = text.split_whitespace().filter(|w| !w.contains("://")).map(|w| w.to_lowercase()).collect();
    if words.iter().any(|w| w.contains('?')) {
        return true;
    }
    let joined = words.join(" ");
    QUESTION_OPENERS
        .iter()
        .any(|o| joined.starts_with(o) && joined[o.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric()))
}

/// Which rule a message from someone else matches, most specific first:
/// my thread, then a topic, then a question. Bots and the user's own
/// messages never match.
pub fn match_rules(
    msg: &Message,
    live: &LiveState,
    store: &Store,
    topics: &[Topic],
    me: Id,
) -> AnyResult<Option<Rule>> {
    if msg.author.id == me || msg.author.bot {
        return Ok(None);
    }
    let in_thread = live.channel(msg.channel_id).is_some_and(|c| c.kind.is_thread());
    let replies_to_me = msg.reply_to_author == Some(me)
        || match msg.reply_to {
            Some(r) => store.is_own_message(r)?,
            None => false,
        };
    if replies_to_me || (in_thread && store.wrote_in(msg.channel_id)?) {
        return Ok(Some(Rule::MyThread));
    }
    if let Some(t) = topic_for(&msg.content, topics) {
        return Ok(Some(Rule::Topic(t.kind.clone())));
    }
    Ok(is_question(&msg.content).then_some(Rule::Question))
}

/// On full watch: a level set in tritone on the channel, else (thread) its
/// parent, else its server, is `Full`. Discord mutes don't matter here.
pub fn full_watch(c: &Channel, watch: &WatchMap) -> bool {
    explicit_watch(c, watch) == Some(WatchLevel::Full)
}

/// Handles one live message:
/// - the user's own: recorded in `own_message` (ids and time); it answers
///   the opportunity it replies to, and in a thread every open one there for
///   an older message (quote or not). In a main channel it closes nothing
///   else;
/// - someone else's, in a full-watch server channel: stored as an open
///   opportunity when a rule matches. A thread keeps one open `my_thread`
///   row, its newest, so one busy thread can't fill the list.
pub fn scan_message(
    store: &Store,
    live: &LiveState,
    topics: &[Topic],
    watch: &WatchMap,
    msg: &Message,
    own: bool,
) -> AnyResult<()> {
    let channel = live.channel(msg.channel_id);
    let guild = msg.guild_id.or_else(|| channel.as_ref().and_then(|c| c.guild_id));
    let at = msg.timestamp_ms;
    if own {
        let row = OwnMessageRow { message_id: msg.id, channel_id: msg.channel_id, guild_id: guild, created_at: at };
        store.record_own_messages(&[row], at)?;
        if let Some(r) = msg.reply_to {
            store.opportunity_answered(r, at)?;
        }
        // In a thread the user is part of the conversation, so their message
        // answers what came before it there. In a main channel it may be
        // unrelated chat: only a reply answers, above.
        if channel.as_ref().is_some_and(|c| c.kind.is_thread()) {
            store.opportunities_answered_before(msg.channel_id, msg.id, at)?;
        }
        return Ok(());
    }
    let (Some(guild), Some(channel), Some(me)) = (guild, channel, live.me()) else { return Ok(()) };
    if !full_watch(&channel, watch) {
        return Ok(());
    }
    if let Some(rule) = match_rules(msg, live, store, topics, me.id)? {
        let my_thread = rule == Rule::MyThread;
        store.add_opportunity(&OpportunityRow {
            message_id: msg.id,
            channel_id: msg.channel_id,
            guild_id: guild,
            rule: rule.to_db(),
            created_at: at,
            status: OpportunityStatus::Open,
            updated_at: at,
        })?;
        if my_thread && channel.kind.is_thread() {
            store.keep_newest_my_thread(msg.channel_id, at)?;
        }
    }
    Ok(())
}

/// Scans every live message. Subscribes before returning, so nothing
/// published after the call is missed. A lagged receiver skips and goes on.
pub fn spawn_scanner(live: Arc<LiveState>, store: Arc<Store>, config: Arc<Config>) -> JoinHandle<()> {
    let mut rx = live.subscribe();
    tokio::spawn(async move {
        let mut watch = WatchMap::default();
        let mut loaded: Option<Instant> = None;
        loop {
            match rx.recv().await {
                Ok(LiveEvent::Message { msg, own }) => {
                    if loaded.is_none_or(|t| t.elapsed() > WATCH_TTL) {
                        match store.all_watch() {
                            Ok(w) => watch = w,
                            Err(e) => tracing::warn!("reading watch levels failed: {e}"),
                        }
                        loaded = Some(Instant::now());
                    }
                    if let Err(e) = scan_message(&store, &live, &config.topics, &watch, &msg, own) {
                        tracing::warn!("opportunity scan failed: {e}");
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => tracing::warn!(skipped, "opportunity scanner lagged"),
                Err(RecvError::Closed) => break,
            }
        }
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Opportunity {
    pub id: Id,
    /// `question`, `topic` or `my_thread`.
    pub rule: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Pass as send_message's `kind` when answering it.
    pub kind: String,
    pub channel: String,
    pub channel_id: Id,
    pub age_mins: i64,
    pub message: RenderedMessage,
    /// Up to 5 messages before it, oldest first.
    pub context: Vec<RenderedMessage>,
    pub link: String,
}

/// Someone started a thread from the message: Discord gives the thread the
/// message's id. https://docs.discord.food/resources/channel#create-thread-from-message
fn thread_started_from(message: Id, live: &LiveState) -> bool {
    live.channel(message).is_some_and(|c| c.kind.is_thread())
}

/// Someone other than the author replied to it, or wrote in a thread started from it.
fn answered_by_others(msg: &Message, later: &[Message], live: &LiveState) -> bool {
    let replied = later.iter().any(|m| m.reply_to == Some(msg.id) && m.author.id != msg.author.id);
    // A thread started from a message takes the message's id.
    let thread = live.recent(msg.id).iter().any(|m| m.author.id != msg.author.id);
    replied || thread
}

/// Open opportunities, newest first, at most `limit`:
/// - from the last `unanswered_max_age_hours`, in channels still on full watch;
/// - a question shows only once it's `unanswered_after_mins` old;
/// - one someone other than its author answered (a reply, or a message in a
///   thread started from it) is marked answered and dropped, as is a question
///   with a thread started from it, and one whose message is gone.
///
/// The message and its context come from the live buffer, else from
/// `around(channel, message, 10)` (e.g. after a restart, or past the buffer's
/// age), at most 10 lookups per call.
pub async fn get_opportunities(ctx: &Ctx, limit: usize) -> Result<Vec<Opportunity>> {
    let now = ctx.timer.now_ms();
    let cutoff = now - i64::from(ctx.config.unanswered_max_age_hours) * 60 * MINUTE_MS;
    let min_question_age = i64::from(ctx.config.unanswered_after_mins) * MINUTE_MS;
    let watch = ctx.store.all_watch()?;
    let mut out = Vec::new();
    let mut fetches = 0;
    for row in ctx.store.open_opportunities(cutoff)? {
        if out.len() >= limit {
            break;
        }
        let Some(rule) = Rule::from_db(&row.rule) else { continue };
        if rule == Rule::Question && now - row.created_at < min_question_age {
            continue;
        }
        let Some(channel) = ctx.live.channel(row.channel_id) else { continue };
        if !full_watch(&channel, &watch) {
            continue;
        }
        // A question someone opened a thread on is being answered there, even
        // when tritone doesn't see the thread's messages.
        if rule == Rule::Question && thread_started_from(row.message_id, &ctx.live) {
            ctx.store.set_opportunity_status(row.message_id, OpportunityStatus::Answered, now)?;
            continue;
        }
        let buffered = ctx.live.recent(row.channel_id);
        let page = if buffered.iter().any(|m| m.id == row.message_id) {
            buffered
        } else {
            if fetches == MAX_FETCHES {
                continue;
            }
            fetches += 1;
            match ctx.api.around(row.channel_id, row.message_id, AROUND_LIMIT).await {
                Ok(mut page) => {
                    page.sort_by_key(|m| m.id);
                    page
                }
                Err(e) if e.halts() => {
                    halt_everything(&ctx.live, &e);
                    return Err(e.into());
                }
                Err(DiscordError::NotFound | DiscordError::Forbidden) => {
                    ctx.store.set_opportunity_status(row.message_id, OpportunityStatus::Gone, now)?;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(kind = e.kind(), "opportunity lookup failed");
                    continue;
                }
            }
        };
        let Some(pos) = page.iter().position(|m| m.id == row.message_id) else {
            ctx.store.set_opportunity_status(row.message_id, OpportunityStatus::Gone, now)?;
            continue;
        };
        let msg = &page[pos];
        if answered_by_others(msg, &page[pos + 1..], &ctx.live) {
            ctx.store.set_opportunity_status(row.message_id, OpportunityStatus::Answered, now)?;
            continue;
        }
        let context =
            page[pos.saturating_sub(CONTEXT_BEFORE)..pos].iter().map(|m| render_message(m, &ctx.live)).collect();
        let topic = match &rule {
            Rule::Topic(k) => Some(k.clone()),
            _ => None,
        };
        let message = render_message(msg, &ctx.live);
        out.push(Opportunity {
            id: row.message_id,
            rule: rule.name(),
            topic,
            kind: rule.kind(),
            channel: channel_label(&channel, &ctx.live),
            channel_id: row.channel_id,
            age_mins: (now - row.created_at) / MINUTE_MS,
            link: message.link.clone(),
            message,
            context,
        });
    }
    Ok(out)
}

/// Hides an opportunity for good.
pub fn dismiss_opportunity(store: &Store, id: Id, now_ms: i64) -> Result<()> {
    set_status(store, id, OpportunityStatus::Dismissed, now_ms)
}

/// The user answered it.
pub fn mark_answered(store: &Store, id: Id, now_ms: i64) -> Result<()> {
    set_status(store, id, OpportunityStatus::Answered, now_ms)
}

fn set_status(store: &Store, id: Id, status: OpportunityStatus, now_ms: i64) -> Result<()> {
    if store.set_opportunity_status(id, status, now_ms)? {
        Ok(())
    } else {
        Err(Error::Invalid(format!("no opportunity with id {id}: use an id from get_opportunities")))
    }
}

#[cfg(test)]
#[path = "opportunities_tests.rs"]
mod tests;
