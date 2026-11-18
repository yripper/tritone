//! Discord JSON objects (users, roles, guilds, channels, settings) to the slim
//! model, for what `discord::parse` doesn't cover. Shared by READY and the
//! dispatch events. Missing or odd fields fall
//! back to defaults: one malformed object never drops the rest.
//!
//! Shapes: https://docs.discord.food/resources/guild,
//! https://docs.discord.food/resources/channel,
//! https://docs.discord.food/resources/user-settings#guild-settings-structure

use std::collections::HashMap;

use serde_json::Value;

use crate::discord::parse;
pub(crate) use crate::discord::parse::parse_user;
use crate::discord::types::{Channel, Guild, GuildSettings, Id, MuteUntil, Role, UserRef};

/// A snowflake sent as a string or a number.
pub(crate) fn id(v: &Value) -> Option<Id> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(Id),
        _ => None,
    }
}

pub(crate) fn id_at(v: &Value, key: &str) -> Option<Id> {
    v.get(key).and_then(id)
}

pub(crate) fn ids_at(v: &Value, key: &str) -> Vec<Id> {
    v.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(id).collect()).unwrap_or_default()
}

/// Permission bitfields come as decimal strings.
fn bits_at(v: &Value, key: &str) -> u64 {
    match v.get(key) {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn bool_at(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub(crate) fn array_at<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// `{entries, partial, version}` (versioned capabilities) or a bare array.
pub(crate) fn entries(v: Option<&Value>) -> &[Value] {
    match v {
        Some(Value::Array(a)) => a,
        Some(o @ Value::Object(_)) => array_at(o, "entries"),
        _ => &[],
    }
}

pub(crate) fn parse_role(v: &Value) -> Option<Role> {
    Some(Role { id: id_at(v, "id")?, permissions: bits_at(v, "permissions") })
}

/// A channel or thread (`parse::parse_channel`), plus what the gateway knows
/// that the object may leave out: `guild` for channels nested in a guild, and
/// DM recipients sent as `recipient_ids` (DEDUPE_USER_OBJECTS), looked up in
/// READY's `users`.
pub(crate) fn parse_channel(v: &Value, guild: Option<Id>, users: &HashMap<Id, UserRef>) -> Option<Channel> {
    let mut c = parse::parse_channel(v)?;
    c.guild_id = c.guild_id.or(guild);
    if c.recipients.is_empty() {
        c.recipients = ids_at(v, "recipient_ids").iter().filter_map(|u| users.get(u).cloned()).collect();
    }
    Some(c)
}

/// A guild, from a plain guild object or a gateway guild whose fields sit in
/// `properties`. Channels are separate (`guild_channels`).
pub(crate) fn parse_guild(v: &Value, my_roles: Vec<Id>) -> Option<Guild> {
    let props = v.get("properties").filter(|p| p.is_object()).unwrap_or(v);
    let roles = if v.get("roles").is_some() { array_at(v, "roles") } else { array_at(props, "roles") };
    Some(Guild {
        id: id_at(v, "id")?,
        name: str_at(props, "name").unwrap_or_default(),
        owner_id: id_at(props, "owner_id").unwrap_or_default(),
        member_count: v.get("member_count").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()),
        large: bool_at(v, "large"),
        roles: roles.iter().filter_map(parse_role).collect(),
        my_roles,
    })
}

/// The guild's channels plus the threads the user has joined: READY and
/// GUILD_CREATE only sync joined threads to user accounts
/// (https://docs.discord.food/gateway/gateway-events#ready).
pub(crate) fn guild_channels(v: &Value, guild: Id) -> Vec<Channel> {
    let none = HashMap::new();
    let channels = array_at(v, "channels").iter().filter_map(|c| parse_channel(c, Some(guild), &none));
    let threads = array_at(v, "threads")
        .iter()
        .filter_map(|c| parse_channel(c, Some(guild), &none))
        .map(|t| Channel { joined: true, ..t });
    channels.chain(threads).collect()
}

/// The user's roles from a guild's member entries (`merged_members`, or
/// GUILD_CREATE `members`), only from the entry that is `me`. Other entries
/// are other people: READY_SUPPLEMENTAL's `merged_members` holds only those
/// (https://docs.discord.food/gateway/gateway-events#ready, note 7).
pub(crate) fn my_roles_in(members: &[Value], me: Id) -> Option<Vec<Id>> {
    members
        .iter()
        .find(|m| id_at(m, "user_id").or_else(|| m.get("user").and_then(|u| id_at(u, "id"))) == Some(me))
        .map(|m| ids_at(m, "roles"))
}

/// A mute is active when `muted` is set, until `mute_config.end_time` if any.
fn mute(v: &Value) -> MuteUntil {
    if !bool_at(v, "muted") {
        return MuteUntil::No;
    }
    let end = v.get("mute_config").and_then(|c| c.get("end_time")).and_then(Value::as_str);
    match end.and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        Some(t) => MuteUntil::Until(t.timestamp_millis()),
        None => MuteUntil::Forever,
    }
}

/// One `user_guild_settings` entry; `guild_id: null` is the DM settings.
pub(crate) fn parse_settings(v: &Value) -> GuildSettings {
    GuildSettings {
        guild_id: id_at(v, "guild_id"),
        muted: mute(v),
        suppress_everyone: bool_at(v, "suppress_everyone"),
        suppress_roles: bool_at(v, "suppress_roles"),
        channel_mutes: array_at(v, "channel_overrides")
            .iter()
            .filter_map(|o| Some((id_at(o, "channel_id")?, mute(o))))
            .collect(),
    }
}
