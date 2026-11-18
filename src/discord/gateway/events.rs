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
    /// GUILD_CREATE / GUILD_UPDATE with the guild's channels.
    GuildUpsert(Box<Guild>, Vec<Channel>),
    GuildDelete(Id),
    SettingsUpdate(GuildSettings),
    /// The socket dropped; the gateway is reconnecting.
    Disconnected,
    /// The gateway stopped and won't retry until the user acts.
    Halted(DiscordError),
}
