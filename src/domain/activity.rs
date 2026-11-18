//! The contribution log: every message the user posts in a server, logged
//! as ids, a time and a kind (never text).

use anyhow::Result as AnyResult;

use crate::config::Topic;
use crate::discord::types::{ChannelKind, Id, Message};
use crate::domain::live::LiveState;
use crate::store::{ActivityRow, Store};

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

/// Logs one of the user's own messages. DMs are not logged. `kind` is an
/// explicit kind (an opportunity rule, or one Claude chose); without one the
/// kind is `other`. `via_tritone` marks a send that went through the queue:
/// the gateway echo may have logged it first, so it flags that row too.
/// Returns whether a row was inserted.
pub fn record_own(
    store: &Store,
    live: &LiveState,
    _topics: &[Topic],
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
        kind: kind.unwrap_or("other").to_string(),
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
