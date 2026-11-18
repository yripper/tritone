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

use crate::discord::types::{Channel, Id, ReadySnapshot};
use crate::store::WatchLevel;

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

/// Channels whose effective watch level is full: the channel's own level,
/// else (for a thread) its parent's, else its guild's.
pub(crate) fn full_watch_channels(channels: &[Channel], watch: &HashMap<Id, WatchLevel>) -> HashSet<Id> {
    channels
        .iter()
        .filter(|c| {
            let parent = if c.kind.is_thread() { c.parent_id } else { None };
            let level = watch
                .get(&c.id)
                .or_else(|| parent.and_then(|p| watch.get(&p)))
                .or_else(|| c.guild_id.and_then(|g| watch.get(&g)));
            level == Some(&WatchLevel::Full)
        })
        .map(|c| c.id)
        .collect()
}

#[cfg(test)]
#[path = "subscribe_tests.rs"]
mod tests;
