//! What the gateway task hands to `LiveState::apply`, already slimmed.

use std::collections::HashMap;

use serde_json::Value;

use super::objects::{
    array_at, guild_channels, id_at, ids_at, my_roles_in, parse_channel, parse_guild, parse_role, parse_settings,
};
use crate::discord::parse::parse_message;

use crate::discord::types::{Channel, DiscordError, Guild, GuildSettings, Id, Message, ReadySnapshot, Role};

#[derive(Debug, Clone)]
pub enum GatewayEvent {
    Ready(Box<ReadySnapshot>),
    Resumed,
    MessageCreate(Box<Message>),
    /// A read done anywhere (phone, app, or tritone itself).
    MessageAck {
        channel: Id,
        message: Id,
        mention_count: Option<u32>,
    },
    ChannelUpsert(Channel),
    ChannelDelete(Id),
    /// GUILD_CREATE: the guild with its full channel set, joined threads
    /// included. Channels not listed are dropped.
    GuildUpsert(Box<Guild>, Vec<Channel>),
    /// GUILD_UPDATE: name, owner and roles. Keeps the channels and the
    /// user's own roles, which GUILD_UPDATE doesn't carry.
    GuildUpdate(Box<Guild>),
    /// The user's own roles in a guild changed (GUILD_MEMBER_UPDATE for self).
    MyRolesUpdate {
        guild: Id,
        roles: Vec<Id>,
    },
    /// GUILD_ROLE_CREATE / GUILD_ROLE_UPDATE: adds or replaces one role.
    RoleUpsert {
        guild: Id,
        role: Role,
    },
    /// GUILD_ROLE_DELETE: drops the role, from the user's roles too.
    RoleDelete {
        guild: Id,
        role: Id,
    },
    GuildDelete(Id),
    SettingsUpdate(GuildSettings),
    /// The socket dropped; the gateway is reconnecting.
    Disconnected,
    /// The gateway stopped and won't retry until the user acts. On close 4004
    /// prefer `LiveState::reject_token`, which also remembers the dead token.
    Halted(DiscordError),
}

/// The model events for one dispatch (`t`, `d`). Most give one event;
/// THREAD_LIST_SYNC gives one per thread; anything tritone doesn't track
/// (SESSIONS_REPLACE, PRESENCE_UPDATE, ...) gives none, silently. `me` picks
/// out the user's own GUILD_MEMBER_UPDATE and member entry.
pub(crate) fn parse_dispatch(t: &str, d: &Value, me: Option<Id>) -> Vec<GatewayEvent> {
    let none = HashMap::new();
    let ev = match t {
        "MESSAGE_CREATE" => parse_message(d).map(|m| GatewayEvent::MessageCreate(Box::new(m))),
        "MESSAGE_ACK" => message_ack(d),
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "THREAD_CREATE" | "THREAD_UPDATE" => {
            parse_channel(d, None, &none).map(GatewayEvent::ChannelUpsert)
        }
        "CHANNEL_DELETE" | "THREAD_DELETE" => id_at(d, "id").map(GatewayEvent::ChannelDelete),
        // The threads the user can see in a guild: upsert each, so they exist
        // as channels before their messages arrive.
        "THREAD_LIST_SYNC" => {
            let guild = id_at(d, "guild_id");
            return array_at(d, "threads")
                .iter()
                .filter_map(|c| parse_channel(c, guild, &none))
                .map(GatewayEvent::ChannelUpsert)
                .collect();
        }
        // `unavailable` is an outage: the guild is still there.
        "GUILD_CREATE" if !unavailable(d) => guild_create(d, me),
        "GUILD_UPDATE" => parse_guild(d, vec![]).map(|g| GatewayEvent::GuildUpdate(Box::new(g))),
        "GUILD_DELETE" if !unavailable(d) => id_at(d, "id").map(GatewayEvent::GuildDelete),
        "GUILD_MEMBER_UPDATE" => my_member_update(d, me),
        "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => role_upsert(d),
        "GUILD_ROLE_DELETE" => role_delete(d),
        "USER_GUILD_SETTINGS_UPDATE" => d.is_object().then(|| GatewayEvent::SettingsUpdate(parse_settings(d))),
        _ => None,
    };
    ev.into_iter().collect()
}

fn unavailable(d: &Value) -> bool {
    d.get("unavailable").and_then(Value::as_bool).unwrap_or(false)
}

fn message_ack(d: &Value) -> Option<GatewayEvent> {
    Some(GatewayEvent::MessageAck {
        channel: id_at(d, "channel_id")?,
        message: id_at(d, "message_id")?,
        mention_count: d.get("mention_count").and_then(Value::as_u64).map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
    })
}

fn guild_create(d: &Value, me: Option<Id>) -> Option<GatewayEvent> {
    let gid = id_at(d, "id")?;
    let roles = me.map(|me| my_roles_in(array_at(d, "members"), me)).unwrap_or_default();
    let g = parse_guild(d, roles)?;
    Some(GatewayEvent::GuildUpsert(Box::new(g), guild_channels(d, gid)))
}

/// Only the user's own member update matters: it changes what they can see.
fn my_member_update(d: &Value, me: Option<Id>) -> Option<GatewayEvent> {
    let user = id_at(d.get("user")?, "id")?;
    if me != Some(user) {
        return None;
    }
    Some(GatewayEvent::MyRolesUpdate { guild: id_at(d, "guild_id")?, roles: ids_at(d, "roles") })
}

fn role_upsert(d: &Value) -> Option<GatewayEvent> {
    Some(GatewayEvent::RoleUpsert { guild: id_at(d, "guild_id")?, role: parse_role(d.get("role")?)? })
}

fn role_delete(d: &Value) -> Option<GatewayEvent> {
    Some(GatewayEvent::RoleDelete { guild: id_at(d, "guild_id")?, role: id_at(d, "role_id")? })
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
