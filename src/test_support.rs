//! Fixture builders for tests. Small fake ids and names like G001, C001, U001.

use std::collections::HashMap;

use crate::discord::types::{
    Channel, ChannelKind, Guild, GuildSettings, Id, Message, MuteUntil, ReadState, ReadySnapshot, Role, UserRef,
};

/// VIEW_CHANNEL.
pub const VIEW: u64 = 1 << 10;

pub fn user(n: u64) -> UserRef {
    UserRef { id: Id(n), username: format!("U{n:03}"), display_name: None, bot: false }
}

/// A guild whose @everyone role (id = guild id) can view channels.
pub fn guild(n: u64) -> Guild {
    Guild {
        id: Id(n),
        name: format!("G{n:03}"),
        owner_id: Id(999),
        member_count: Some(10),
        large: false,
        roles: vec![Role { id: Id(n), name: "@everyone".into(), permissions: VIEW }],
        my_roles: vec![],
    }
}

pub fn text_channel(n: u64, guild: u64) -> Channel {
    Channel {
        id: Id(n),
        guild_id: Some(Id(guild)),
        parent_id: None,
        name: Some(format!("C{n:03}")),
        kind: ChannelKind::Text,
        recipients: vec![],
        last_message_id: None,
        overwrites: vec![],
        position: 0,
        joined: false,
    }
}

pub fn dm(n: u64, with: u64) -> Channel {
    Channel {
        id: Id(n),
        guild_id: None,
        parent_id: None,
        name: None,
        kind: ChannelKind::Dm,
        recipients: vec![user(with)],
        last_message_id: None,
        overwrites: vec![],
        position: 0,
        joined: false,
    }
}

/// A message whose timestamp comes from its id.
pub fn msg(id: Id, channel: u64, author: u64, content: &str) -> Message {
    Message {
        id,
        channel_id: Id(channel),
        guild_id: None,
        author: user(author),
        content: content.to_string(),
        timestamp_ms: id.timestamp_ms(),
        edited: false,
        reply_to: None,
        reply_to_author: None,
        mentions: vec![],
        mention_roles: vec![],
        mention_everyone: false,
        attachments: vec![],
        embeds: vec![],
        reactions: vec![],
        kind: 0,
    }
}

/// A message id at a given time.
pub fn id_at(ms: i64) -> Id {
    Id::from_timestamp_ms(ms, 0)
}

pub fn ready(me: u64, guilds: Vec<Guild>, channels: Vec<Channel>) -> ReadySnapshot {
    ReadySnapshot {
        me: user(me),
        session_id: "s".into(),
        resume_url: "wss://example.invalid".into(),
        guilds,
        channels,
        read_states: HashMap::new(),
        settings: vec![],
        users: vec![],
    }
}

pub fn read_state(last_acked: Option<Id>, mention_count: u32) -> ReadState {
    ReadState { last_acked, mention_count }
}

pub fn settings(guild: Option<u64>, muted: MuteUntil) -> GuildSettings {
    GuildSettings {
        guild_id: guild.map(Id),
        muted,
        suppress_everyone: false,
        suppress_roles: false,
        channel_mutes: HashMap::new(),
    }
}
