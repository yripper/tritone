//! Reading: history (which marks read unless peeking), listings, channel
//! info, search and people.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::discord::types::{Channel, ChannelKind, Id, SearchQuery, SearchScope};
use crate::domain::errors::{Error, Result};
use crate::domain::live::LiveState;
use crate::domain::paced_search::SearchPacer;
use crate::domain::pending::{effective_watch, listable};
use crate::domain::render::{RenderedMessage, render_message, rfc3339};
use crate::domain::resolve::channel_label;
use crate::mcp::state::Ctx;
use crate::store::{Store, WatchLevel, WatchMap};

#[derive(Debug, Clone, Serialize)]
pub struct History {
    pub channel_label: String,
    /// Oldest first.
    pub messages: Vec<RenderedMessage>,
    pub newest_id: Option<String>,
    /// Older messages exist: pass the oldest id as `before`.
    pub more: bool,
    /// Reading worked but marking read failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ack_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KindFilter {
    Dm,
    GroupDm,
    Server,
    Thread,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub id: Id,
    pub name: String,
    /// Set in tritone; `None` = the default.
    pub watch: Option<WatchLevel>,
    pub discord_muted: bool,
    pub unread_channels: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelInfo {
    pub id: Id,
    pub label: String,
    pub kind: &'static str,
    /// What applies, after channel, server and Discord mutes.
    pub watch: WatchLevel,
    /// Set in tritone on this channel; `None` = inherited or default.
    pub explicit_watch: Option<WatchLevel>,
    pub discord_muted: bool,
    pub unread: bool,
    pub mention_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_message_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserMatch {
    pub id: Id,
    pub label: String,
    pub username: String,
    /// The 1:1 DM with them, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dm_channel: Option<Id>,
    /// Servers where tritone saw them write recently (no member lists are kept).
    pub seen_in_servers: Vec<String>,
}

/// The newest `limit` messages (1..=100), or those before `before`. Without
/// `peek` and `before`, marks the channel read up to the newest one returned.
/// `read_thread` is this, on a thread's id.
pub async fn read_history(ctx: &Ctx, channel: Id, limit: u8, before: Option<Id>, peek: bool) -> Result<History> {
    let limit = limit.clamp(1, 100);
    let page = ctx.api.history(channel, before, None, limit).await?;
    ctx.live.note_authors(&page);
    let newest = page.first().map(|m| m.id);
    let mut ack_error = None;
    if let Some(n) = newest.filter(|_| !peek && before.is_none()) {
        let acked = ctx.live.read_state(channel).last_acked;
        if acked.is_none_or(|a| n > a) {
            // The messages were read either way: report a failed ack instead of
            // losing them. A halting error already stopped the client.
            match ctx.api.ack(channel, n).await {
                Ok(()) => ctx.live.note_acked(channel, n),
                Err(e) => ack_error = Some(e.to_string()),
            }
        }
    }
    let channel_label = ctx.live.channel(channel).map_or_else(|| channel.to_string(), |c| channel_label(&c, &ctx.live));
    Ok(History {
        channel_label,
        messages: page.iter().rev().map(|m| render_message(m, &ctx.live)).collect(),
        newest_id: newest.map(|n| n.to_string()),
        more: page.len() == usize::from(limit),
        ack_error,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub total: u32,
    /// Newest first, 25 per page.
    pub messages: Vec<RenderedMessage>,
}

/// Discord's own search, in one server or one channel (DMs included),
/// through `pacer` so it never runs alongside another search (backfill,
/// voice, cleanup).
pub async fn search(ctx: &Ctx, pacer: &SearchPacer, scope: SearchScope, query: &SearchQuery) -> Result<SearchResult> {
    let page = pacer.search(ctx, scope, query).await?;
    ctx.live.note_authors(&page.messages);
    Ok(SearchResult {
        total: page.total,
        messages: page.messages.iter().map(|m| render_message(m, &ctx.live)).collect(),
    })
}

fn unread_channels(live: &LiveState, watch: &WatchMap, guild: Id) -> u32 {
    live.channels()
        .iter()
        .filter(|c| c.guild_id == Some(guild) && listable(c, watch) && live.can_view(c.id) && live.is_unread(c.id))
        .count() as u32
}

/// Servers by name. Servers muted in tritone are left out unless asked for;
/// servers muted in Discord are listed and flagged.
pub fn list_servers(live: &LiveState, store: &Store, now_ms: i64, include_muted: bool) -> Result<Vec<ServerInfo>> {
    let watch = store.all_watch()?;
    let mut out: Vec<ServerInfo> = live
        .guilds()
        .into_iter()
        .filter(|g| include_muted || watch.guilds.get(&g.id) != Some(&WatchLevel::Muted))
        .map(|g| ServerInfo {
            id: g.id,
            watch: watch.guilds.get(&g.id).copied(),
            discord_muted: live.settings(Some(g.id)).is_some_and(|s| s.muted.active(now_ms)),
            unread_channels: unread_channels(live, &watch, g.id),
            name: g.name,
        })
        .collect();
    out.sort_by_key(|s| s.name.to_lowercase());
    Ok(out)
}

fn kind_name(k: ChannelKind) -> &'static str {
    match k {
        ChannelKind::Text => "text",
        ChannelKind::Announcement => "announcement",
        ChannelKind::Forum => "forum",
        ChannelKind::Media => "media",
        ChannelKind::PublicThread => "public_thread",
        ChannelKind::PrivateThread => "private_thread",
        ChannelKind::AnnouncementThread => "announcement_thread",
        ChannelKind::Dm => "dm",
        ChannelKind::GroupDm => "group_dm",
        ChannelKind::Voice => "voice",
        ChannelKind::Category => "category",
        ChannelKind::Other(_) => "other",
    }
}

fn kind_matches(c: &Channel, kind: KindFilter) -> bool {
    match kind {
        KindFilter::Dm => c.kind == ChannelKind::Dm,
        KindFilter::GroupDm => c.kind == ChannelKind::GroupDm,
        KindFilter::Server => c.guild_id.is_some() && !c.kind.is_thread(),
        KindFilter::Thread => c.kind.is_thread(),
    }
}

fn info(c: &Channel, live: &LiveState, store: &Store, watch: &WatchMap, now_ms: i64) -> Result<ChannelInfo> {
    Ok(ChannelInfo {
        id: c.id,
        label: channel_label(c, live),
        kind: kind_name(c.kind),
        watch: effective_watch(c, live, watch, now_ms),
        explicit_watch: watch.channels.get(&c.id).copied(),
        discord_muted: live.discord_muted(c.id, now_ms),
        unread: live.is_unread(c.id),
        mention_count: live.read_state(c.id).mention_count,
        snoozed_until: store.snoozed_until(c.id)?.filter(|t| *t > now_ms).map(rfc3339),
        last_message_at: c.last_message_id.map(|m| rfc3339(m.timestamp_ms())),
    })
}

/// Visible channels, DMs and group DMs first, then by label. Muted ones (in
/// tritone or Discord) are left out unless `watch` asks for them; categories
/// and nameless server channels never show.
pub fn list_channels(
    live: &LiveState,
    store: &Store,
    kind: Option<KindFilter>,
    server: Option<Id>,
    watch: Option<WatchLevel>,
    now_ms: i64,
) -> Result<Vec<ChannelInfo>> {
    let levels = store.all_watch()?;
    let mut out = Vec::new();
    for c in live.channels() {
        if !listable(&c, &levels) || !live.can_view(c.id) || kind.is_some_and(|k| !kind_matches(&c, k)) {
            continue;
        }
        if server.is_some_and(|s| c.guild_id != Some(s)) {
            continue;
        }
        let level = effective_watch(&c, live, &levels, now_ms);
        let wanted = match watch {
            Some(w) => level == w,
            None => level != WatchLevel::Muted,
        };
        if wanted {
            out.push(info(&c, live, store, &levels, now_ms)?);
        }
    }
    out.sort_by_key(|i| (!(i.kind == "dm" || i.kind == "group_dm"), i.label.to_lowercase()));
    Ok(out)
}

/// One channel's details, hidden or muted ones included.
pub fn channel_info(live: &LiveState, store: &Store, channel: Id, now_ms: i64) -> Result<ChannelInfo> {
    let c = live.channel(channel).ok_or_else(|| Error::NotFound(channel.to_string()))?;
    info(&c, live, store, &store.all_watch()?, now_ms)
}

/// The most people `find_user` returns.
const MAX_USERS: usize = 25;

/// Known people (READY's users, DM recipients, authors seen) whose username
/// or display name contains `query`, case-insensitively; whole-name matches
/// first. No member search over REST: it would look like scraping.
pub fn find_user(live: &LiveState, query: &str) -> Vec<UserMatch> {
    let q = query.trim().trim_start_matches('@').to_lowercase();
    if q.is_empty() {
        return vec![];
    }
    let me = live.me().map(|u| u.id);
    let channels = live.channels();
    let mut seen: HashMap<Id, BTreeSet<String>> = HashMap::new();
    for c in channels.iter().filter(|c| c.guild_id.is_some()) {
        let Some(g) = c.guild_id.and_then(|g| live.guild(g)) else { continue };
        for m in live.recent(c.id) {
            seen.entry(m.author.id).or_default().insert(g.name.clone());
        }
    }
    let mut found: Vec<(bool, UserMatch)> = live
        .users()
        .into_iter()
        .filter(|u| Some(u.id) != me)
        .filter_map(|u| {
            let names = [Some(u.username.to_lowercase()), u.display_name.as_deref().map(str::to_lowercase)];
            let names: Vec<String> = names.into_iter().flatten().collect();
            if !names.iter().any(|n| n.contains(&q)) {
                return None;
            }
            let exact = names.contains(&q);
            let dm_channel = channels
                .iter()
                .find(|c| c.kind == ChannelKind::Dm && c.recipients.iter().any(|r| r.id == u.id))
                .map(|c| c.id);
            let seen_in_servers = seen.remove(&u.id).map(|s| s.into_iter().collect()).unwrap_or_default();
            Some((
                exact,
                UserMatch { id: u.id, label: u.label().to_string(), username: u.username, dm_channel, seen_in_servers },
            ))
        })
        .collect();
    found.sort_by_key(|(exact, u)| (!*exact, u.label.to_lowercase(), u.id));
    found.into_iter().take(MAX_USERS).map(|(_, u)| u).collect()
}

#[cfg(test)]
#[path = "reading_tests.rs"]
mod tests;
