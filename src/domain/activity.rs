//! The contribution log: every message the user posts in a server, logged
//! as ids, a time and a kind (never text). Live messages come from the
//! gateway through `spawn_recorder`; older ones from a slow, resumable
//! `backfill` over Discord search.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result as AnyResult;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::config::{Config, Topic};
use crate::discord::types::{Channel, ChannelKind, DiscordError, Id, Message, SearchQuery, SearchScope};
use crate::domain::errors::{Error, Result};
use crate::domain::live::{ConnState, LiveEvent, LiveState};
use crate::domain::paced_search::SearchPacer;
use crate::mcp::state::Ctx;
use crate::store::{ActivityRow, Store};

/// Discord search refuses offsets past this; deeper pages use a `max_id` cursor.
/// https://docs.discord.food/resources/message#search-guild-messages
const MAX_SEARCH_OFFSET: u32 = 9_975;
/// Links listed by `my_activity`.
const MAX_LINKS: usize = 50;

/// `https://discord.com/channels/{guild|@me}/{channel}/{message}`.
pub fn message_link(guild: Option<Id>, channel: Id, message: Id) -> String {
    let g = guild.map_or_else(|| "@me".to_string(), |g| g.to_string());
    format!("https://discord.com/channels/{g}/{channel}/{message}")
}

/// `Server › #channel`, `Server › #parent › thread`, `@user (DM)` or
/// `a, b (group DM)`. Unknown parts fall back to placeholders, never to ids
/// that look like names.
pub fn channel_label(live: &LiveState, channel: Id) -> String {
    let Some(c) = live.channel(channel) else { return format!("unknown channel {channel}") };
    let name = c.name.clone().unwrap_or_else(|| "unknown-channel".into());
    match c.kind {
        ChannelKind::Dm => {
            let who = c.recipients.first().map_or("unknown-user", |u| u.label());
            format!("@{who} (DM)")
        }
        ChannelKind::GroupDm => {
            let names: Vec<&str> = c.recipients.iter().map(|u| u.label()).collect();
            let shown = c.name.clone().unwrap_or_else(|| names.join(", "));
            format!("{shown} (group DM)")
        }
        _ => {
            let server = c.guild_id.and_then(|g| live.guild(g)).map_or("unknown server".to_string(), |g| g.name);
            match c.parent_id.and_then(|p| live.channel(p)).filter(|_| c.kind.is_thread()) {
                Some(parent) => {
                    let pname = parent.name.unwrap_or_else(|| "unknown-channel".into());
                    format!("{server} › #{pname} › {name}")
                }
                None => format!("{server} › #{name}"),
            }
        }
    }
}

/// The message's server: its own `guild_id`, else its channel's (REST
/// message objects usually omit it), else `fallback`.
fn guild_of(live: &LiveState, msg: &Message, fallback: Option<Id>) -> Option<Id> {
    msg.guild_id.or_else(|| live.channel(msg.channel_id).and_then(|c| c.guild_id)).or(fallback)
}

/// `keyword` occurs in `text` as a whole word (or phrase), ignoring case.
fn has_word(text: &str, keyword: &str) -> bool {
    let (text, kw) = (text.to_lowercase(), keyword.trim().to_lowercase());
    if kw.is_empty() {
        return false;
    }
    let boundary = |c: Option<char>| !c.is_some_and(char::is_alphanumeric);
    text.match_indices(&kw)
        .any(|(at, _)| boundary(text[..at].chars().next_back()) && boundary(text[at + kw.len()..].chars().next()))
}

/// The first topic with a keyword in `text`.
pub fn topic_for<'a>(text: &str, topics: &'a [Topic]) -> Option<&'a Topic> {
    topics.iter().find(|t| t.keywords.iter().any(|k| has_word(text, k)))
}

/// A question mark, or a sentence that opens like a question.
pub fn looks_like_question(text: &str) -> bool {
    const OPENERS: &[&str] = &[
        "how", "what", "why", "when", "where", "which", "who", "does", "do", "is", "are", "can", "could", "should",
        "would", "anyone", "has", "have", "any",
    ];
    let first = text.split_whitespace().next().unwrap_or_default().to_lowercase();
    text.contains('?') || OPENERS.contains(&first.trim_matches(|c: char| !c.is_alphanumeric()))
}

/// The contribution kind of one of the user's messages:
/// - a topic keyword in the message, or in the message it replies to, gives the topic's kind;
/// - so does a reply in a channel or thread whose name (or its parent's) matches a topic;
/// - a reply to a question gives `answer`;
/// - anything else is `other`.
///
/// The replied-to message is looked up in the live buffer only (full-watch
/// channels): no REST call per message.
pub fn classify(msg: &Message, channel: Option<&Channel>, live: &LiveState, topics: &[Topic]) -> String {
    if let Some(t) = topic_for(&msg.content, topics) {
        return t.kind.clone();
    }
    let replied = msg.reply_to.and_then(|r| live.recent(msg.channel_id).into_iter().find(|m| m.id == r));
    if let Some(t) = replied.as_ref().and_then(|m| topic_for(&m.content, topics)) {
        return t.kind.clone();
    }
    if msg.reply_to.is_some() {
        let parent = channel.and_then(|c| c.parent_id).and_then(|p| live.channel(p));
        let names = [channel.and_then(|c| c.name.clone()), parent.and_then(|p| p.name)];
        if let Some(t) = names.iter().flatten().find_map(|n| topic_for(n, topics)) {
            return t.kind.clone();
        }
    }
    if replied.is_some_and(|m| looks_like_question(&m.content)) {
        return "answer".into();
    }
    "other".into()
}

/// Logs one of the user's own messages. DMs are not logged. `kind` is an
/// explicit kind (an opportunity rule, or one Claude chose); without one the
/// kind is inferred by `classify`. `via_tritone` marks a send that went through the queue:
/// the gateway echo may have logged it first, so it flags that row too.
/// Returns whether a row was inserted.
pub fn record_own(
    store: &Store,
    live: &LiveState,
    topics: &[Topic],
    msg: &Message,
    guild_hint: Option<Id>,
    kind: Option<&str>,
    via_tritone: bool,
) -> AnyResult<bool> {
    let Some(guild) = guild_of(live, msg, guild_hint) else { return Ok(false) };
    let row = ActivityRow {
        message_id: msg.id,
        guild_id: guild,
        channel_id: msg.channel_id,
        created_at: msg.timestamp_ms,
        kind: match kind {
            Some(k) => k.to_string(),
            None => classify(msg, live.channel(msg.channel_id).as_ref(), live, topics),
        },
        is_reply: msg.reply_to.is_some(),
        via_tritone,
    };
    let inserted = store.record_activity(&row)?;
    if !inserted {
        if via_tritone {
            store.mark_sent_via_tritone(msg.id)?;
        }
        if let Some(k) = kind {
            store.set_activity_kind(msg.id, k)?;
        }
    }
    Ok(inserted)
}

/// Logs every message the user posts in a server, from any device, as the
/// gateway sees it. Subscribes before returning, so nothing published after
/// the call is missed.
pub fn spawn_recorder(live: Arc<LiveState>, store: Arc<Store>, config: Arc<Config>) -> JoinHandle<()> {
    let mut rx = live.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(LiveEvent::Message { msg, own: true }) => {
                    if let Err(e) = record_own(&store, &live, &config.topics, &msg, None, None, false) {
                        tracing::warn!("recording own message failed: {e}");
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => tracing::warn!(skipped, "activity recorder lagged"),
                Err(RecvError::Closed) => break,
            }
        }
    })
}

/// Backfill progress for one guild, kept in `kv` under `<key>:<guild>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GuildProgress {
    /// The next search offset, under `max_id` when set.
    offset: u32,
    /// Past the offset limit: only messages older than this.
    max_id: Option<Id>,
    /// Discord's hit count for the current cursor.
    total: u32,
    done: bool,
    /// Messages paged through so far, across runs.
    #[serde(default)]
    seen: u32,
}

fn progress_key(key: &str, guild: Id) -> String {
    format!("{key}:{guild}")
}

fn load_progress(store: &Store, key: &str) -> Result<GuildProgress> {
    // A corrupt entry restarts that guild; rows are insert-or-ignore anyway.
    Ok(store.kv_get(key)?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

fn save_progress(store: &Store, key: &str, p: &GuildProgress) -> Result<()> {
    let v = serde_json::to_string(p).map_err(|e| Error::Store(e.into()))?;
    Ok(store.kv_set(key, &v)?)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackfillReport {
    pub guilds_done: usize,
    pub guilds_total: usize,
    /// New rows logged by this run.
    pub recorded: usize,
    /// Messages paged through so far, across runs.
    pub messages_seen: u64,
}

fn guild_ids(live: &LiveState) -> Vec<Id> {
    let mut ids: Vec<Id> = live.guilds().into_iter().map(|g| g.id).collect();
    ids.sort();
    ids
}

/// Where a backfill stands, without searching.
pub fn backfill_progress(store: &Store, live: &LiveState, key: &str) -> Result<BackfillReport> {
    let guilds = guild_ids(live);
    let mut report = BackfillReport { guilds_done: 0, guilds_total: guilds.len(), recorded: 0, messages_seen: 0 };
    for g in guilds {
        let p = load_progress(store, &progress_key(key, g))?;
        report.guilds_done += usize::from(p.done);
        report.messages_seen += u64::from(p.seen);
    }
    Ok(report)
}

/// Logs the user's older server messages, guild by guild, through Discord
/// search (`author_id` = me, 25 per page) through the shared `SearchPacer`,
/// so it never runs alongside another search: 3–6 s between searches, an
/// index that isn't ready or a rate limit waited out by the pacer.
/// Resumable: progress lives in `kv`. A halting error halts tritone and
/// stops the run; any other error stops it with progress kept.
pub async fn backfill(ctx: &Ctx, progress_key: &str) -> Result<BackfillReport> {
    backfill_with(ctx, progress_key, SearchPacer::global()).await
}

pub async fn backfill_with(ctx: &Ctx, key: &str, pacer: &SearchPacer) -> Result<BackfillReport> {
    let me = ctx.live.me().ok_or(Error::NotConnected)?.id;
    let mut recorded = 0;
    for g in guild_ids(&ctx.live) {
        let pkey = progress_key(key, g);
        let mut p = load_progress(&ctx.store, &pkey)?;
        while !p.done {
            // Halted elsewhere (gateway, a tool): stop before the next search, keeping progress.
            if let ConnState::Halted(e) = ctx.live.connection() {
                tracing::warn!(kind = e.kind(), "backfill stopped: tritone is halted");
                return Err(e.into());
            }
            let q = SearchQuery { author_id: Some(me), offset: p.offset, max_id: p.max_id, ..SearchQuery::default() };
            match pacer.search(ctx, SearchScope::Guild(g), &q).await {
                Ok(page) => {
                    for m in &page.messages {
                        recorded += usize::from(record_own(
                            &ctx.store,
                            &ctx.live,
                            &ctx.config.topics,
                            m,
                            Some(g),
                            None,
                            false,
                        )?);
                    }
                    let n = page.messages.len() as u32;
                    p.seen += n;
                    p.offset += n;
                    p.total = page.total;
                    if n == 0 || p.offset >= page.total {
                        p.done = true;
                    } else if p.offset > MAX_SEARCH_OFFSET {
                        p.max_id = page.messages.iter().map(|m| m.id).min();
                        p.offset = 0;
                    }
                    save_progress(&ctx.store, &pkey, &p)?;
                }
                // No access to search there (or the server is gone): skip it.
                Err(Error::Discord(DiscordError::Forbidden | DiscordError::NotFound)) => {
                    p.done = true;
                    save_progress(&ctx.store, &pkey, &p)?;
                }
                // The pacer already halted tritone on a halting error.
                Err(e) => {
                    tracing::warn!(kind = e.kind(), "backfill stopped");
                    return Err(e);
                }
            }
        }
    }
    let mut report = backfill_progress(&ctx.store, &ctx.live, key)?;
    report.recorded = recorded;
    Ok(report)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Count {
    pub label: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivityLink {
    pub link: String,
    pub at: String,
    pub kind: String,
    pub server: String,
    pub channel: String,
    pub is_reply: bool,
    pub via_tritone: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivitySummary {
    pub from: String,
    pub to: String,
    pub total: usize,
    pub by_kind: Vec<Count>,
    pub by_server: Vec<Count>,
    pub by_channel: Vec<Count>,
    /// Newest first, at most 50.
    pub messages: Vec<ActivityLink>,
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| ms.to_string())
}

/// Most first, then by label.
fn counts(labels: impl Iterator<Item = String>) -> Vec<Count> {
    let mut m: HashMap<String, usize> = HashMap::new();
    for l in labels {
        *m.entry(l).or_default() += 1;
    }
    let mut v: Vec<Count> = m.into_iter().map(|(label, count)| Count { label, count }).collect();
    v.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
    v
}

/// What the user posted in servers between `from_ms` and `to_ms`: counts by
/// kind, server and channel, and links to the newest 50 messages.
pub fn my_activity(store: &Store, live: &LiveState, from_ms: i64, to_ms: i64) -> Result<ActivitySummary> {
    let rows = store.activity_between(from_ms, to_ms)?;
    let server = |g: Id| live.guild(g).map_or_else(|| "unknown server".to_string(), |g| g.name);
    let channels: HashMap<Id, String> =
        rows.iter().map(|r| (r.channel_id, channel_label(live, r.channel_id))).collect();
    let messages = rows
        .iter()
        .take(MAX_LINKS)
        .map(|r| ActivityLink {
            link: message_link(Some(r.guild_id), r.channel_id, r.message_id),
            at: rfc3339(r.created_at),
            kind: r.kind.clone(),
            server: server(r.guild_id),
            channel: channels[&r.channel_id].clone(),
            is_reply: r.is_reply,
            via_tritone: r.via_tritone,
        })
        .collect();
    Ok(ActivitySummary {
        from: rfc3339(from_ms),
        to: rfc3339(to_ms),
        total: rows.len(),
        by_kind: counts(rows.iter().map(|r| r.kind.clone())),
        by_server: counts(rows.iter().map(|r| server(r.guild_id))),
        by_channel: counts(rows.iter().map(|r| channels[&r.channel_id].clone())),
        messages,
    })
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
