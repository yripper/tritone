//! What's waiting on the user: which channels are pending and why.

use serde::Serialize;

use crate::discord::types::{Channel, ChannelKind, GuildSettings, Id, Message};
use crate::domain::errors::Result;
use crate::domain::live::LiveState;
use crate::domain::render::{RenderedMessage, render_message};
use crate::domain::resolve::channel_label;
use crate::mcp::state::Ctx;
use crate::store::{Store, WatchLevel, WatchMap};

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
/// History calls one `get_pending` may make in all, counting channels that
/// turn out read (no read state, the user's message last).
const MAX_CALLS: usize = 30;
/// Pinging messages shown per mention or reply entry.
const MAX_PINGS: usize = 5;
/// Mentions inbox pages of 100 read by `get_pending`, at most.
const MAX_INBOX_PAGES: usize = 5;
const INBOX_PAGE: u8 = 100;

/// The watch level that applies: an explicit level on the channel (for a
/// thread, then on its parent), then on its server; otherwise a Discord mute
/// means `Muted`, and DMs and group DMs are `Full`, everything else `Normal`.
pub fn effective_watch(channel: &Channel, live: &LiveState, watch: &WatchMap, now_ms: i64) -> WatchLevel {
    watch_of(channel, live, watch, now_ms).0
}

/// The level set in tritone that applies: the channel's own, else (for a
/// thread) its parent's, else its server's. Needs no live model, so the
/// gateway can use it before READY is applied.
pub fn explicit_watch(c: &Channel, watch: &WatchMap) -> Option<WatchLevel> {
    let parent = c.parent_id.filter(|_| c.kind.is_thread());
    watch
        .channels
        .get(&c.id)
        .or_else(|| parent.and_then(|p| watch.channels.get(&p)))
        .or_else(|| c.guild_id.and_then(|g| watch.guilds.get(&g)))
        .copied()
}

/// The level, and whether it was set in tritone.
fn watch_of(c: &Channel, live: &LiveState, watch: &WatchMap, now_ms: i64) -> (WatchLevel, bool) {
    if let Some(level) = explicit_watch(c, watch) {
        return (level, true);
    }
    if live.discord_muted(c.id, now_ms) {
        (WatchLevel::Muted, false)
    } else if c.kind.is_private() {
        (WatchLevel::Full, false)
    } else {
        (WatchLevel::Normal, false)
    }
}

/// Channels that show up in pending and listings: no categories, no server
/// channel without a name (a phantom made by a message in a channel tritone
/// never saw), and no thread the user hasn't joined (they see every active
/// thread, like Discord, which doesn't count those as unread either) unless
/// tritone's watch on it is full: in a help forum on full watch every
/// question is a thread the user never joined, and those must show. A direct
/// mention or reply in an unjoined thread still reaches pending, as an
/// unlisted ping.
pub(crate) fn listable(c: &Channel, watch: &WatchMap) -> bool {
    let theirs = !c.kind.is_thread() || c.joined || explicit_watch(c, watch) == Some(WatchLevel::Full);
    c.kind != ChannelKind::Category && (c.guild_id.is_none() || c.name.is_some()) && theirs
}

/// What the unread messages in hand (buffered, plus the mentions inbox) say.
#[derive(Default)]
struct Pings {
    direct: u32,
    replies: u32,
    /// A role or @everyone/@here ping the server doesn't suppress.
    group: bool,
}

/// How a message pings the user, if at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ping {
    Direct,
    Reply,
    /// A role or @everyone/@here ping the server doesn't suppress.
    Group,
}

fn ping_of(m: &Message, me: Id, settings: Option<&GuildSettings>, my_roles: &[Id]) -> Option<Ping> {
    if m.mentions.contains(&me) {
        return Some(Ping::Direct);
    }
    if m.reply_to_author == Some(me) {
        return Some(Ping::Reply);
    }
    let roles = !settings.is_some_and(|s| s.suppress_roles) && m.mention_roles.iter().any(|r| my_roles.contains(r));
    let everyone = m.mention_everyone && !settings.is_some_and(|s| s.suppress_everyone);
    (roles || everyone).then_some(Ping::Group)
}

/// The messages among `msgs` that ping the user in channel `c`.
fn pinging<'a>(c: &Channel, msgs: &'a [Message], me: Id, live: &LiveState) -> Vec<(Ping, &'a Message)> {
    let settings = live.settings(c.guild_id);
    let my_roles = c.guild_id.and_then(|g| live.guild(g)).map(|g| g.my_roles).unwrap_or_default();
    msgs.iter().filter_map(|m| ping_of(m, me, settings.as_ref(), &my_roles).map(|p| (p, m))).collect()
}

fn pings(c: &Channel, msgs: &[Message], me: Id, live: &LiveState) -> Pings {
    let mut p = Pings::default();
    for (kind, _) in pinging(c, msgs, me, live) {
        match kind {
            Ping::Direct => p.direct += 1,
            Ping::Reply => p.replies += 1,
            Ping::Group => p.group = true,
        }
    }
    p
}

/// Unread messages in hand for a channel: buffered ones plus inbox entries,
/// newer than `last_acked`.
fn in_hand(c: &Channel, live: &LiveState, mentions: &[Message]) -> (Vec<Message>, Vec<Message>) {
    let acked = live.read_state(c.id).last_acked;
    let is_new = |m: &Message| m.channel_id == c.id && acked.is_none_or(|a| m.id > a);
    let buffered: Vec<Message> = live.recent(c.id).into_iter().filter(|m| is_new(m)).collect();
    let mut all = buffered.clone();
    for m in mentions.iter().filter(|m| is_new(m)) {
        if !all.iter().any(|b| b.id == m.id) {
            all.push(m.clone());
        }
    }
    (buffered, all)
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
        if !listable(&c, &watch) || !live.can_view(c.id) || !live.is_unread(c.id) {
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
        let (buffered, in_hand) = in_hand(&c, live, mentions);
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
    found.extend(unlisted_pings(live, store, &watch, mentions, me, now_ms)?);
    found.sort_by_key(|(group, newest, _)| (*group, std::cmp::Reverse(*newest)));
    Ok(found.into_iter().map(|(_, _, item)| item).collect())
}

/// Label of an entry for a channel tritone doesn't hold (or holds without a
/// name): an archived or unknown thread, usually.
const UNLISTED: &str = "(thread or unknown channel)";

/// Whether `pending` handles the channel itself, or it's unknown or nameless.
fn unlisted(live: &LiveState, watch: &WatchMap, channel: Id) -> bool {
    live.channel(channel).is_none_or(|c| !listable(&c, watch))
}

/// The inbox's direct mentions and replies to the user in unlisted
/// channels: a direct ping is always pending, wherever it is. Hidden only by
/// an explicit tritone mute (channel or server), a snooze, or a read state
/// past the message.
fn unlisted_pings(
    live: &LiveState,
    store: &Store,
    watch: &WatchMap,
    mentions: &[Message],
    me: Id,
    now_ms: i64,
) -> Result<Vec<(u8, Option<Id>, PendingItem)>> {
    let mut by_channel: Vec<(Id, Vec<&Message>)> = Vec::new();
    for m in mentions.iter().filter(|m| unlisted(live, watch, m.channel_id)) {
        let direct = m.mentions.contains(&me) || m.reply_to_author == Some(me);
        let read = live.read_state(m.channel_id).last_acked.is_some_and(|a| a >= m.id);
        if !direct || read {
            continue;
        }
        match by_channel.iter_mut().find(|(c, _)| *c == m.channel_id) {
            Some((_, msgs)) => msgs.push(m),
            None => by_channel.push((m.channel_id, vec![m])),
        }
    }
    let mut out = Vec::new();
    for (channel, msgs) in by_channel {
        let guild = msgs[0].guild_id.or_else(|| live.channel(channel).and_then(|c| c.guild_id));
        let muted = |level: Option<&WatchLevel>| level == Some(&WatchLevel::Muted);
        if muted(watch.channels.get(&channel)) || muted(guild.and_then(|g| watch.guilds.get(&g))) {
            continue;
        }
        if store.snoozed_until(channel)?.is_some_and(|t| t > now_ms) {
            continue;
        }
        let direct = msgs.iter().filter(|m| m.mentions.contains(&me)).count() as u32;
        let server = guild.and_then(|g| live.guild(g)).map_or_else(|| "unknown-server".to_string(), |g| g.name);
        // An unjoined thread tritone knows has a real name.
        let label = match live.channel(channel).filter(|c| c.name.is_some()) {
            Some(c) => channel_label(&c, live),
            None => format!("{server} › {UNLISTED}"),
        };
        let item = PendingItem {
            channel,
            label,
            reason: if direct > 0 { Reason::Mention } else { Reason::Reply },
            unread_count_hint: msgs.len() as u32,
            mention_count: msgs.len() as u32,
        };
        out.push((1, msgs.iter().map(|m| m.id).max(), item));
    }
    Ok(out)
}

/// What `get_pending` returns.
#[derive(Debug, Clone, Serialize)]
pub struct PendingReport {
    pub pending: Vec<PendingEntry>,
    /// The mentions inbox wasn't read whole for its 7 days (more than
    /// `MAX_INBOX_PAGES` pages, or a page failed): older mentions in
    /// Discord-muted channels may be missing.
    pub truncated: bool,
}

/// The mentions inbox of the last 7 days, newest first, and whether the page
/// cap or a failed page stopped it. Role and @everyone pings share the inbox, so a direct
/// mention can sit several pages down.
async fn inbox(ctx: &Ctx, now: i64) -> Result<(Vec<Message>, bool)> {
    let since = now - MENTIONS_WINDOW_MS;
    let (mut all, mut before) = (Vec::<Message>::new(), None);
    for _ in 0..MAX_INBOX_PAGES {
        let page = match ctx.api.recent_mentions(INBOX_PAGE, before).await {
            Ok(p) => p,
            Err(e) if e.halts() => return Err(e.into()),
            // Read states and the buffer still work without (the rest of) the
            // inbox, but the 7 days weren't read whole: say so.
            Err(e) => {
                tracing::warn!(kind = e.kind(), "recent mentions failed");
                return Ok((all, true));
            }
        };
        let full = page.len() == usize::from(INBOX_PAGE);
        let past_window = page.iter().any(|m| m.timestamp_ms < since);
        before = page.iter().map(|m| m.id).min();
        all.extend(page.into_iter().filter(|m| m.timestamp_ms >= since));
        if !full || past_window {
            return Ok((all, false));
        }
    }
    Ok((all, true))
}

/// `pending` plus messages for each entry: for mentions and replies, the
/// pinging messages already in hand; for the rest (and mentions with none in
/// hand), the first unread messages. Never marks anything read.
pub async fn get_pending(ctx: &Ctx) -> Result<PendingReport> {
    let now = ctx.timer.now_ms();
    let (mentions, truncated) = inbox(ctx, now).await?;
    ctx.live.note_authors(&mentions);
    let Some(me) = ctx.live.me().map(|u| u.id) else { return Ok(PendingReport { pending: vec![], truncated }) };
    let watch = ctx.store.all_watch()?;
    let (mut kept, mut calls) = (0, 0);
    let mut out = Vec::new();
    for item in pending(&ctx.live, &ctx.store, &mentions, now)? {
        if unlisted(&ctx.live, &watch, item.channel) {
            // Only the inbox's messages are known: show them, with their links.
            let mut msgs: Vec<&Message> = mentions.iter().filter(|m| m.channel_id == item.channel).collect();
            msgs.sort_by_key(|m| m.id);
            let messages = msgs.into_iter().map(|m| render_message(m, &ctx.live)).collect();
            out.push(PendingEntry { item, messages, more_unread: true, error: None });
            continue;
        }
        // A mention or reply shows the messages that ping the user, from the
        // inbox and the buffer, at no REST cost: the oldest unread would be
        // channel chatter. The rest is for read_history.
        if matches!(item.reason, Reason::Mention | Reason::Reply) {
            let shown = pings_in_hand(&ctx.live, &item, &mentions, me);
            if !shown.is_empty() {
                out.push(PendingEntry { item, messages: shown, more_unread: true, error: None });
                continue;
            }
        }
        let acked = ctx.live.read_state(item.channel).last_acked;
        // A channel with no read state must be fetched to know whether it's
        // unread at all, so it's fetched even past the budget of kept entries.
        let fetch = calls < MAX_CALLS && (kept < MAX_FETCH || acked.is_none());
        if !fetch {
            out.push(PendingEntry { item, messages: vec![], more_unread: true, error: None });
            continue;
        }
        calls += 1;
        let page = match ctx.api.history(item.channel, None, acked, FIRST_UNREAD as u8 + 1).await {
            Ok(p) => p,
            Err(e) if e.halts() => return Err(e.into()),
            Err(e) => {
                out.push(PendingEntry { item, messages: vec![], more_unread: true, error: Some(e.to_string()) });
                continue;
            }
        };
        ctx.live.note_authors(&page);
        // No read state at all (a DM never opened here or elsewhere): it's
        // unread only if the last message isn't the user's, as in Discord.
        if acked.is_none() && page.first().is_some_and(|m| m.author.id == me) {
            continue;
        }
        kept += 1;
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
    Ok(PendingReport { pending: out, truncated })
}

/// Up to `MAX_PINGS` of the unread messages in hand that ping the user,
/// oldest first: direct mentions and replies before role and @everyone pings,
/// newest first within each.
fn pings_in_hand(live: &LiveState, item: &PendingItem, mentions: &[Message], me: Id) -> Vec<RenderedMessage> {
    let Some(c) = live.channel(item.channel) else { return vec![] };
    let (_, in_hand) = in_hand(&c, live, mentions);
    let mut found = pinging(&c, &in_hand, me, live);
    found.sort_by_key(|(kind, m)| (*kind == Ping::Group, std::cmp::Reverse(m.id)));
    found.truncate(MAX_PINGS);
    let mut keep: Vec<&Message> = found.into_iter().map(|(_, m)| m).collect();
    keep.sort_by_key(|m| m.id);
    keep.into_iter().map(|m| render_message(m, live)).collect()
}

#[cfg(test)]
#[path = "pending_tests.rs"]
mod tests;
