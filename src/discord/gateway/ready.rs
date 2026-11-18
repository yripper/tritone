//! READY (and READY_SUPPLEMENTAL) slimmed into a `ReadySnapshot`. The caller
//! drops the JSON right after: the parsed Value is ~10 MiB, the slim model a
//! fraction of that.
//!
//! Fields: https://docs.discord.food/gateway/gateway-events#ready,
//! https://docs.discord.food/topics/read-state#read-state-structure

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::GatewayEvent;
use super::objects::{
    array_at, entries, guild_channels, id_at, my_roles_in, parse_channel, parse_guild, parse_settings, parse_user,
};
use crate::discord::types::{Id, ReadState, ReadySnapshot, UserRef};

/// Channel read states; types 1–5 are badges for other things.
const READ_STATE_CHANNEL: u64 = 0;

pub(crate) fn parse_ready(ready: &Value, supplemental: Option<&Value>) -> ReadySnapshot {
    let me = ready.get("user").and_then(parse_user).unwrap_or_else(|| UserRef {
        id: Id(0),
        username: String::new(),
        display_name: None,
        bot: false,
    });

    let users: HashMap<Id, UserRef> = array_at(ready, "users")
        .iter()
        .chain(array_at(ready, "relationships").iter().filter_map(|r| r.get("user")))
        .filter_map(parse_user)
        .map(|u| (u.id, u))
        .collect();

    // merged_members is index-aligned with `guilds`; with
    // PRIORITIZED_READY_PAYLOAD it may only come in READY_SUPPLEMENTAL.
    let merged = array_at(ready, "merged_members");
    let supp_roles: HashMap<Id, Vec<Id>> = supplemental.map(|s| roles_by_guild(s, me.id)).unwrap_or_default();

    let mut guilds = Vec::new();
    let mut channels = Vec::new();
    for (i, g) in array_at(ready, "guilds").iter().enumerate() {
        let Some(gid) = id_at(g, "id") else { continue };
        let my_roles = merged
            .get(i)
            .and_then(Value::as_array)
            .and_then(|members| my_roles_in(members, me.id))
            .or_else(|| supp_roles.get(&gid).cloned())
            .unwrap_or_default();
        let Some(guild) = parse_guild(g, my_roles) else { continue };
        channels.extend(guild_channels(g, gid));
        guilds.push(guild);
    }
    channels.extend(array_at(ready, "private_channels").iter().filter_map(|c| parse_channel(c, None, &users)));

    let read_states = entries(ready.get("read_state"))
        .iter()
        .filter(|e| {
            e.get("read_state_type").and_then(Value::as_u64).unwrap_or(READ_STATE_CHANNEL) == READ_STATE_CHANNEL
        })
        .filter_map(|e| {
            let last_acked = id_at(e, "last_message_id").filter(|m| m.0 != 0);
            let mention_count = e.get("mention_count").and_then(Value::as_u64).unwrap_or(0);
            Some((
                id_at(e, "id")?,
                ReadState { last_acked, mention_count: u32::try_from(mention_count).unwrap_or(u32::MAX) },
            ))
        })
        .collect();

    // Discord keeps settings for guilds the user has left.
    let joined: HashSet<Id> = guilds.iter().map(|g| g.id).collect();
    let settings = entries(ready.get("user_guild_settings"))
        .iter()
        .map(parse_settings)
        .filter(|s| s.guild_id.is_none_or(|g| joined.contains(&g)))
        .collect();

    ReadySnapshot {
        me,
        session_id: ready.get("session_id").and_then(Value::as_str).unwrap_or_default().to_string(),
        resume_url: ready.get("resume_gateway_url").and_then(Value::as_str).unwrap_or_default().to_string(),
        guilds,
        channels,
        read_states,
        settings,
        users: users.into_values().collect(),
    }
}

/// READY_SUPPLEMENTAL's `merged_members`, index-aligned with its `guilds`.
fn roles_by_guild(supp: &Value, me: Id) -> HashMap<Id, Vec<Id>> {
    let merged = array_at(supp, "merged_members");
    array_at(supp, "guilds")
        .iter()
        .zip(merged)
        .filter_map(|(g, m)| Some((id_at(g, "id")?, my_roles_in(m.as_array()?, me)?)))
        .collect()
}

/// The user's roles per guild from READY_SUPPLEMENTAL, only where it carries
/// the user's own member (normally READY does, and this is empty).
pub(crate) fn supplemental_roles(supp: &Value, me: Id) -> Vec<GatewayEvent> {
    let mut by_guild: Vec<(Id, Vec<Id>)> = roles_by_guild(supp, me).into_iter().collect();
    by_guild.sort_by_key(|(g, _)| *g);
    by_guild.into_iter().map(|(guild, roles)| GatewayEvent::MyRolesUpdate { guild, roles }).collect()
}

#[cfg(test)]
#[path = "ready_tests.rs"]
mod tests;
