//! Writing in the user's voice: one tone profile per category and notes per
//! channel, both written by Claude. tritone never stores message text: the
//! user's own messages are fetched live through Discord search when Claude
//! needs to learn, and staleness is counted from message ids and times only
//! (`own_message`).
//!
//! A channel's category is its own, else (for a thread) its parent
//! channel's, else its server's, else `general`.

use std::collections::HashSet;

use serde::Serialize;

use crate::discord::types::{Id, Message, SearchQuery, SearchScope};
use crate::domain::errors::{Error, Result};
use crate::domain::live::LiveState;
use crate::domain::paced_search::SearchPacer;
use crate::mcp::state::Ctx;
use crate::store::{OwnMessageRow, Store};

/// A profile or notes entry is stale after this many own messages or this long.
pub const STALE_OWN_MESSAGES: u32 = 30;
pub const STALE_AGE_SECS: i64 = 21 * 86_400;
pub const CHAT_SAMPLE: usize = 60;
pub const CATEGORY_SAMPLE: usize = 40;
/// At most this many other channels (or servers) are searched for a category sample.
pub const CATEGORY_THREADS: usize = 3;
pub const DEFAULT_CATEGORY: &str = "general";
pub const SUGGESTED_CATEGORIES: &[&str] = &["dev-community", "work", "friends", "general"];
pub const PROFILE_MAX: usize = 4_000;
pub const NOTES_MAX: usize = 2_000;
/// Each page is a paced search (25 hits): bound how far back a sample digs.
const OWN_MAX_PAGES: usize = 3;
/// How many recent channels to look through for a category sample.
const RECENT_CHANNELS_SCAN: usize = 200;

pub fn is_stale(updated_at_ms: i64, own_since: u32, now_ms: i64) -> bool {
    own_since >= STALE_OWN_MESSAGES || now_ms - updated_at_ms >= STALE_AGE_SECS * 1000
}

/// `^[a-z0-9_-]{1,32}$`
pub fn valid_category(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn check_category(s: &str) -> Result<()> {
    if valid_category(s) {
        return Ok(());
    }
    Err(Error::Invalid(format!("category must match ^[a-z0-9_-]{{1,32}}$, e.g. {}", SUGGESTED_CATEGORIES.join(", "))))
}

fn check_len(what: &str, body: &str, max: usize) -> Result<()> {
    let n = body.chars().count();
    if n == 0 || n > max {
        return Err(Error::Invalid(format!("{what} must be 1 to {max} characters (got {n})")));
    }
    Ok(())
}

/// The channel's category: its own, its parent's for a thread, its server's,
/// else `general`. `guild_hint` is the server for a channel the live model
/// no longer knows (e.g. a message seen in an archived thread).
pub fn category_of(store: &Store, live: &LiveState, channel: Id, guild_hint: Option<Id>) -> Result<String> {
    let c = live.channel(channel);
    let parent = c.as_ref().filter(|c| c.kind.is_thread()).and_then(|c| c.parent_id);
    let guild = c.as_ref().and_then(|c| c.guild_id).or(guild_hint);
    for target in [Some(channel), parent, guild].into_iter().flatten() {
        if let Some(cat) = store.category(target)? {
            return Ok(cat);
        }
    }
    Ok(DEFAULT_CATEGORY.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VoiceContext {
    pub category: String,
    pub profile: Option<String>,
    pub notes: Option<String>,
    pub profile_stale: bool,
    /// Also true when the channel has no notes yet: they need writing.
    pub notes_stale: bool,
    /// The category profile is missing or stale.
    pub needs_learning: bool,
}

/// Own messages (not sent through tritone) in a category since `since_ms`,
/// by each channel's current category.
pub fn own_count_in_category(store: &Store, live: &LiveState, category: &str, since_ms: i64) -> Result<u32> {
    let mut n = 0;
    for (channel, guild) in store.own_since(since_ms)? {
        if category_of(store, live, channel, guild)? == category {
            n += 1;
        }
    }
    Ok(n)
}

pub fn profile_stale(store: &Store, live: &LiveState, category: &str, updated_at: i64, now_ms: i64) -> Result<bool> {
    Ok(is_stale(updated_at, own_count_in_category(store, live, category, updated_at)?, now_ms))
}

pub fn voice_context(store: &Store, live: &LiveState, channel: Id, now_ms: i64) -> Result<VoiceContext> {
    let category = category_of(store, live, channel, None)?;
    let profile = store.profile(&category)?;
    let notes = store.notes(channel)?;
    let profile_stale = match &profile {
        Some(p) => profile_stale(store, live, &category, p.updated_at, now_ms)?,
        None => false,
    };
    let notes_stale = match &notes {
        Some(n) => is_stale(n.updated_at, store.own_count_in_channel(channel, n.updated_at)?, now_ms),
        None => true,
    };
    Ok(VoiceContext {
        needs_learning: profile.is_none() || profile_stale,
        category,
        profile: profile.map(|p| p.body),
        notes: notes.map(|n| n.body),
        profile_stale,
        notes_stale,
    })
}

/// Categories whose saved profile is due for a refresh.
pub fn stale_profiles(store: &Store, live: &LiveState, now_ms: i64) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (category, updated_at) in store.profile_dates()? {
        if profile_stale(store, live, &category, updated_at, now_ms)? {
            out.push(category);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OwnMessage {
    pub channel_id: Id,
    pub id: Id,
    pub at: String,
    pub text: String,
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| ms.to_string())
}

/// Remembers which own messages exist (ids and times, no text) so staleness
/// can be counted later.
pub fn observe_own(store: &Store, msgs: &[Message], me: Id, guild_hint: Option<Id>, now_ms: i64) -> Result<()> {
    let rows: Vec<OwnMessageRow> = msgs
        .iter()
        .filter(|m| m.author.id == me)
        .map(|m| OwnMessageRow {
            message_id: m.id,
            channel_id: m.channel_id,
            guild_id: m.guild_id.or(guild_hint),
            created_at: m.timestamp_ms,
        })
        .collect();
    if !rows.is_empty() {
        store.record_own_messages(&rows, now_ms)?;
    }
    Ok(())
}

/// The user's own messages with text, excluding what tritone sent for them.
pub fn own_texts<'a>(msgs: &'a [Message], me: Id, sent: &HashSet<Id>) -> Vec<&'a Message> {
    msgs.iter().filter(|m| m.author.id == me && !sent.contains(&m.id) && !m.content.trim().is_empty()).collect()
}

/// Where to search for the user's messages in one channel: the server with a
/// channel filter, or the DM itself.
fn channel_scope(live: &LiveState, channel: Id) -> Result<(SearchScope, Option<Id>, Option<Id>)> {
    let c = live.channel(channel).ok_or_else(|| Error::NotFound(channel.to_string()))?;
    Ok(match c.guild_id {
        Some(g) => (SearchScope::Guild(g), Some(channel), Some(g)),
        None => (SearchScope::Channel(channel), None, None),
    })
}

/// Up to `limit` own messages, newest first, from one search scope, paged
/// live (at most `OWN_MAX_PAGES` searches). `keep` filters hits.
async fn own_from_search(
    ctx: &Ctx,
    pacer: &SearchPacer,
    scope: SearchScope,
    channel_filter: Option<Id>,
    guild: Option<Id>,
    limit: usize,
    keep: &(dyn Fn(&Message) -> Result<bool> + Sync),
) -> Result<Vec<OwnMessage>> {
    let me = ctx.live.me().ok_or(Error::NotConnected)?.id;
    let now = ctx.timer.now_ms();
    let mut out = Vec::new();
    let mut offset = 0;
    for _ in 0..OWN_MAX_PAGES {
        let q = SearchQuery { author_id: Some(me), channel_id: channel_filter, offset, ..SearchQuery::default() };
        let page = pacer.search(ctx, scope, &q).await?;
        observe_own(&ctx.store, &page.messages, me, guild, now)?;
        let ids: Vec<Id> = page.messages.iter().map(|m| m.id).collect();
        let sent = ctx.store.sent_via_tritone(&ids)?;
        for m in own_texts(&page.messages, me, &sent) {
            if keep(m)? {
                out.push(OwnMessage {
                    channel_id: m.channel_id,
                    id: m.id,
                    at: rfc3339(m.timestamp_ms),
                    text: m.content.clone(),
                });
            }
        }
        offset += page.messages.len() as u32;
        if out.len() >= limit || page.messages.is_empty() || offset >= page.total {
            break;
        }
    }
    out.truncate(limit);
    Ok(out)
}

/// Up to `limit` own messages from one channel, newest first, fetched live.
pub async fn own_messages(ctx: &Ctx, pacer: &SearchPacer, channel: Id, limit: usize) -> Result<Vec<OwnMessage>> {
    let (scope, filter, guild) = channel_scope(&ctx.live, channel)?;
    own_from_search(ctx, pacer, scope, filter, guild, limit, &|_| Ok(true)).await
}

/// Up to `limit` own messages from at most `CATEGORY_THREADS` other places of
/// the category: first the channels the user wrote in most recently, then
/// channels and servers set to the category (a server is searched as a
/// whole, keeping only hits whose channel is in the category).
pub async fn own_messages_in_category(
    ctx: &Ctx,
    pacer: &SearchPacer,
    category: &str,
    except: Id,
    limit: usize,
) -> Result<Vec<OwnMessage>> {
    let (store, live) = (&ctx.store, &ctx.live);
    let mut places: Vec<Id> = Vec::new();
    for (channel, guild) in store.recent_own_channels(RECENT_CHANNELS_SCAN)? {
        if places.len() >= CATEGORY_THREADS {
            break;
        }
        if channel != except && live.channel(channel).is_some() && category_of(store, live, channel, guild)? == category
        {
            places.push(channel);
        }
    }
    for target in store.targets_in_category(category)? {
        if places.len() >= CATEGORY_THREADS {
            break;
        }
        let known = live.guild(target).is_some() || live.channel(target).is_some();
        if target != except && known && !places.contains(&target) {
            places.push(target);
        }
    }
    let mut out = Vec::new();
    for place in places {
        if out.len() >= limit {
            break;
        }
        let want = limit - out.len();
        let got = if live.guild(place).is_some() && live.channel(place).is_none() {
            let keep = |m: &Message| -> Result<bool> {
                Ok(m.channel_id != except && category_of(store, live, m.channel_id, Some(place))? == category)
            };
            own_from_search(ctx, pacer, SearchScope::Guild(place), None, Some(place), want, &keep).await?
        } else {
            own_messages(ctx, pacer, place, want).await?
        };
        out.extend(got);
    }
    out.truncate(limit);
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub struct VoiceReport {
    #[serde(flatten)]
    pub context: VoiceContext,
    /// Up to 60, fetched live, when the profile or notes need (re)writing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub my_messages_in_channel: Option<Vec<OwnMessage>>,
    /// Up to 40 from other channels of the category, when the profile needs learning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub my_messages_in_category: Option<Vec<OwnMessage>>,
    pub suggested_categories: &'static [&'static str],
}

/// The voice context for one channel, plus the user's own messages to learn
/// from when the profile or the notes are missing or stale.
pub async fn get_voice(ctx: &Ctx, pacer: &SearchPacer, channel: Id) -> Result<VoiceReport> {
    let now = ctx.timer.now_ms();
    let context = voice_context(&ctx.store, &ctx.live, channel, now)?;
    let mut report = VoiceReport {
        my_messages_in_channel: None,
        my_messages_in_category: None,
        suggested_categories: SUGGESTED_CATEGORIES,
        context,
    };
    if report.context.needs_learning || report.context.notes_stale {
        report.my_messages_in_channel = Some(own_messages(ctx, pacer, channel, CHAT_SAMPLE).await?);
    }
    if report.context.needs_learning {
        let cat = report.context.category.clone();
        report.my_messages_in_category =
            Some(own_messages_in_category(ctx, pacer, &cat, channel, CATEGORY_SAMPLE).await?);
    }
    Ok(report)
}

pub fn save_profile(store: &Store, category: &str, body: &str, now_ms: i64) -> Result<()> {
    check_category(category)?;
    check_len("profile", body, PROFILE_MAX)?;
    store.save_profile(category, body, now_ms)?;
    Ok(())
}

pub fn save_chat_notes(store: &Store, channel: Id, body: &str, now_ms: i64) -> Result<()> {
    check_len("notes", body, NOTES_MAX)?;
    store.save_notes(channel, body, now_ms)?;
    Ok(())
}

/// Sets the category of a channel or server (`target`).
pub fn set_category(store: &Store, target: Id, category: &str) -> Result<()> {
    check_category(category)?;
    store.set_category(target, category)?;
    Ok(())
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
