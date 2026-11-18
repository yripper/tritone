//! Lazy guild subscriptions, sent only where messages went missing.
//!
//! Discord delivers MESSAGE_CREATE for every guild without a subscription on
//! the accounts measured so far, so tritone subscribes to nothing by default.
//! After a fresh READY that follows a reconnect it compares each guild
//! channel's `last_message_id` with what arrived live; only guilds with a gap
//! get op 37, for their full-watch channels. Never member lists.
//! https://docs.discord.food/gateway/opcodes-and-close-codes (op 37, Guild Subscriptions Bulk)

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::discord::types::{Channel, ChannelKind, Id, ReadySnapshot};
use crate::domain::pending::explicit_watch;
use crate::store::{WatchLevel, WatchMap};

/// At most this many channels per guild in op 37. Each costs a member-list
/// range, and a payload over 15 KiB closes the socket with 4002
/// (https://docs.discord.food/gateway/using-gateway).
pub(crate) const MAX_SUBSCRIBED_CHANNELS: usize = 5;
/// Opcode 37, Guild Subscriptions Bulk.
const OP_GUILD_SUBSCRIPTIONS_BULK: u8 = 37;

/// Guilds where a channel's READY `last_message_id` is past the highest id
/// seen live (`seen`, by channel) yet older than `cutoff`, the moment the old
/// socket went down: a message posted while connected that never arrived.
/// Messages from the downtime itself are expected gaps. Channels with no
/// baseline and DMs are skipped.
pub(crate) fn missed_guilds(snapshot: &ReadySnapshot, seen: &HashMap<Id, Id>, cutoff: Id) -> Vec<Id> {
    let mut guilds: Vec<Id> = snapshot
        .channels
        .iter()
        .filter_map(|c| {
            let guild = c.guild_id?;
            let (last, before) = (c.last_message_id?, *seen.get(&c.id)?);
            (last > before && last < cutoff).then_some(guild)
        })
        .collect::<HashSet<Id>>()
        .into_iter()
        .collect();
    guilds.sort();
    guilds
}

/// Op 37 for one guild: typing, threads and activities for these channels'
/// first 100 member-list rows, no member updates. The field set the web
/// client sends when a guild gets focus.
pub(crate) fn subscription_packet(guild: Id, channels: &[Id]) -> Value {
    let ranges: serde_json::Map<String, Value> = channels.iter().map(|c| (c.to_string(), json!([[0, 99]]))).collect();
    json!({
        "op": OP_GUILD_SUBSCRIPTIONS_BULK,
        "d": {"subscriptions": {guild.to_string(): {
            "typing": true,
            "threads": true,
            "activities": true,
            "member_updates": false,
            "members": [],
            "channels": ranges,
            "thread_member_lists": [],
        }}}
    })
}

/// Which of a guild's channels go in its op 37: watched channels that hold
/// messages (text, announcement, forum, media; not categories, voice or
/// threads), the most recently active first, at most `MAX_SUBSCRIBED_CHANNELS`.
pub(crate) fn subscribe_channels(channels: &[Channel], guild: Id, watched: &HashSet<Id>) -> Vec<Id> {
    let mut picked: Vec<&Channel> = channels
        .iter()
        .filter(|c| c.guild_id == Some(guild) && watched.contains(&c.id))
        .filter(|c| {
            matches!(c.kind, ChannelKind::Text | ChannelKind::Announcement | ChannelKind::Forum | ChannelKind::Media)
        })
        .collect();
    picked.sort_by_key(|c| std::cmp::Reverse(c.last_message_id));
    picked.into_iter().take(MAX_SUBSCRIBED_CHANNELS).map(|c| c.id).collect()
}

/// Channels set to full watch in tritone: the channel's own level, else (for
/// a thread) its parent's, else its server's (`pending::explicit_watch`, the
/// rule pending and listings use).
pub(crate) fn full_watch_channels(channels: &[Channel], watch: &WatchMap) -> HashSet<Id> {
    channels.iter().filter(|c| explicit_watch(c, watch) == Some(WatchLevel::Full)).map(|c| c.id).collect()
}

#[cfg(test)]
#[path = "subscribe_tests.rs"]
mod tests;
