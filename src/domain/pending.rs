//! What's waiting on the user: which channels are pending and why.

use std::collections::HashMap;

use serde::Serialize;

use crate::discord::types::{Channel, ChannelKind, Id, Message};
use crate::domain::errors::Result;
use crate::domain::live::LiveState;
use crate::domain::render::{RenderedMessage, render_message};
use crate::domain::resolve::channel_label;
use crate::mcp::state::Ctx;
use crate::store::{Store, WatchLevel};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Dm,
    GroupDm,
    Mention,
    Reply,
    FullWatch,
    Thread,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingItem {
    pub channel: Id,
    pub label: String,
    pub reason: Reason,
    pub unread_count_hint: u32,
    pub mention_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingEntry {
    #[serde(flatten)]
    pub item: PendingItem,
    pub messages: Vec<RenderedMessage>,
    pub more_unread: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// How far back Discord's recent-mentions inbox reaches.
const MENTIONS_WINDOW_MS: i64 = 7 * 86_400_000;
/// `get_pending` shows at most this many unread messages per channel.
const FIRST_UNREAD: usize = 20;
/// `get_pending` fetches messages for at most this many channels (one REST
/// call each); later entries come without messages.
const MAX_FETCH: usize = 15;

/// The watch level that applies: an explicit level on the channel (for a
/// thread, then on its parent), then on its server; otherwise a Discord mute
/// means `Muted`, and DMs and group DMs are `Full`, everything else `Normal`.
pub fn effective_watch(
    channel: &Channel,
    live: &LiveState,
    watch: &HashMap<Id, WatchLevel>,
    now_ms: i64,
) -> WatchLevel {
    watch_of(channel, live, watch, now_ms).0
}

/// The level, and whether it was set in tritone.
fn watch_of(c: &Channel, live: &LiveState, watch: &HashMap<Id, WatchLevel>, now_ms: i64) -> (WatchLevel, bool) {
    let parent = c.parent_id.filter(|_| c.kind.is_thread());
    let explicit = watch
        .get(&c.id)
        .or_else(|| parent.and_then(|p| watch.get(&p)))
        .or_else(|| c.guild_id.and_then(|g| watch.get(&g)));
    if let Some(level) = explicit {
        return (*level, true);
    }
    if live.discord_muted(c.id, now_ms) {
        (WatchLevel::Muted, false)
    } else if c.kind.is_private() {
        (WatchLevel::Full, false)
    } else {
        (WatchLevel::Normal, false)
    }
}

/// Channels that show up in pending and listings: no categories, and no
/// server channel without a name (a phantom made by a message in a channel
/// tritone never saw).
pub(crate) fn listable(c: &Channel) -> bool {
    c.kind != ChannelKind::Category && (c.guild_id.is_none() || c.name.is_some())
}

/// What the unread messages in hand (buffered, plus the mentions inbox) say.
#[derive(Default)]
struct Pings {
    direct: u32,
    replies: u32,
    /// A role or @everyone/@here ping the server doesn't suppress.
    group: bool,
}

fn pings(c: &Channel, msgs: &[Message], me: Id, live: &LiveState) -> Pings {
    let settings = live.settings(c.guild_id);
    let my_roles = c.guild_id.and_then(|g| live.guild(g)).map(|g| g.my_roles).unwrap_or_default();
    let mut p = Pings::default();
    for m in msgs {
        if m.mentions.contains(&me) {
            p.direct += 1;
        } else if m.reply_to_author == Some(me) {
            p.replies += 1;
        }
        let roles = !settings.as_ref().is_some_and(|s| s.suppress_roles)
            && m.mention_roles.iter().any(|r| my_roles.contains(r));
        let everyone = m.mention_everyone && !settings.as_ref().is_some_and(|s| s.suppress_everyone);
        p.group |= roles || everyone;
    }
    p
}

/// The pending channels, ordered DMs and group DMs first, then mentions and
/// replies, then full-watch channels and threads, newest first within each.
/// `mentions` is the recent-mentions inbox (7 days); entries for channels
/// already read are ignored.
///
/// A Discord mute without an explicit tritone level hides everything but a
/// direct mention or a reply to the user, judged from the messages
/// themselves: Discord's `mention_count` also counts role and @everyone
/// pings, so it can't tell them apart.
pub fn pending(live: &LiveState, store: &Store, mentions: &[Message], now_ms: i64) -> Result<Vec<PendingItem>> {
    let Some(me) = live.me().map(|u| u.id) else { return Ok(vec![]) };
    let watch = store.all_watch()?;
    let mut found: Vec<(u8, Option<Id>, PendingItem)> = Vec::new();
    for c in live.channels() {
        if !listable(&c) || !live.can_view(c.id) || !live.is_unread(c.id) {
            continue;
        }
        if store.snoozed_until(c.id)?.is_some_and(|t| t > now_ms) {
            continue;
        }
        let (level, explicit) = watch_of(&c, live, &watch, now_ms);
        if explicit && level == WatchLevel::Muted {
            continue;
        }
        let rs = live.read_state(c.id);
        let is_new = |m: &&Message| m.channel_id == c.id && rs.last_acked.is_none_or(|a| m.id > a);
        let buffered: Vec<Message> = live.recent(c.id).into_iter().filter(|m| is_new(&m)).collect();
        let mut in_hand = buffered.clone();
        for m in mentions.iter().filter(is_new) {
            if !in_hand.iter().any(|b| b.id == m.id) {
                in_hand.push(m.clone());
            }
        }
        let p = pings(&c, &in_hand, me, live);
        let reason = if level == WatchLevel::Muted {
            match (p.direct, p.replies) {
                (0, 0) => continue,
                (0, _) => Reason::Reply,
                _ => Reason::Mention,
            }
        } else if c.kind == ChannelKind::Dm {
            Reason::Dm
        } else if c.kind == ChannelKind::GroupDm {
            Reason::GroupDm
        } else if p.direct > 0 {
            Reason::Mention
        } else if p.replies > 0 {
            Reason::Reply
        } else if p.group || rs.mention_count > 0 {
            Reason::Mention
        } else if level == WatchLevel::Full {
            Reason::FullWatch
        } else if c.kind.is_thread() {
            Reason::Thread
        } else {
            continue;
        };
        let mention_count =
            if level == WatchLevel::Muted { p.direct + p.replies } else { rs.mention_count.max(p.direct + p.replies) };
        let group = match reason {
            Reason::Dm | Reason::GroupDm => 0,
            Reason::Mention | Reason::Reply => 1,
            Reason::FullWatch | Reason::Thread => 2,
        };
        let item = PendingItem {
            channel: c.id,
            label: channel_label(&c, live),
            reason,
            unread_count_hint: buffered.len() as u32,
            mention_count,
        };
        found.push((group, c.last_message_id, item));
    }
    found.sort_by_key(|(group, newest, _)| (*group, std::cmp::Reverse(*newest)));
    Ok(found.into_iter().map(|(_, _, item)| item).collect())
}

/// `pending` plus the first unread messages of each entry. Never marks
/// anything read.
pub async fn get_pending(ctx: &Ctx) -> Result<Vec<PendingEntry>> {
    let now = ctx.timer.now_ms();
    let mentions = match ctx.api.recent_mentions(100, None).await {
        Ok(m) => m.into_iter().filter(|m| m.timestamp_ms >= now - MENTIONS_WINDOW_MS).collect(),
        Err(e) if e.halts() => return Err(e.into()),
        // Read states and the buffer still work without the inbox.
        Err(e) => {
            tracing::warn!(kind = e.kind(), "recent mentions failed");
            vec![]
        }
    };
    let me = ctx.live.me().map(|u| u.id);
    let mut out = Vec::new();
    for (i, item) in pending(&ctx.live, &ctx.store, &mentions, now)?.into_iter().enumerate() {
        if i >= MAX_FETCH {
            out.push(PendingEntry { item, messages: vec![], more_unread: true, error: None });
            continue;
        }
        let acked = ctx.live.read_state(item.channel).last_acked;
        let page = match ctx.api.history(item.channel, None, acked, FIRST_UNREAD as u8 + 1).await {
            Ok(p) => p,
            Err(e) if e.halts() => return Err(e.into()),
            Err(e) => {
                out.push(PendingEntry { item, messages: vec![], more_unread: true, error: Some(e.to_string()) });
                continue;
            }
        };
        // No read state at all (a DM never opened here or elsewhere): it's
        // unread only if the last message isn't the user's, as in Discord.
        if acked.is_none() && page.first().is_some_and(|m| Some(m.author.id) == me) {
            continue;
        }
        let more_unread = page.len() > FIRST_UNREAD;
        // Newest first. After `acked` these are the oldest unread: drop the
        // newest extra one. Without `acked` they're the newest: drop the oldest.
        let keep: Vec<&Message> = match acked {
            Some(_) => page.iter().skip(page.len().saturating_sub(FIRST_UNREAD)).collect(),
            None => page.iter().take(FIRST_UNREAD).collect(),
        };
        let messages = keep.into_iter().rev().map(|m| render_message(m, &ctx.live)).collect();
        out.push(PendingEntry { item, messages, more_unread, error: None });
    }
    Ok(out)
}

#[cfg(test)]
#[path = "pending_tests.rs"]
mod tests;
