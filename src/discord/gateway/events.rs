//! What the gateway task hands to `LiveState::apply`, already slimmed.

use crate::discord::types::{Channel, DiscordError, Guild, GuildSettings, Id, Message, ReadySnapshot};

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
    GuildDelete(Id),
    SettingsUpdate(GuildSettings),
    /// The socket dropped; the gateway is reconnecting.
    Disconnected,
    /// The gateway stopped and won't retry until the user acts. On close 4004
    /// prefer `LiveState::reject_token`, which also remembers the dead token.
    Halted(DiscordError),
}
