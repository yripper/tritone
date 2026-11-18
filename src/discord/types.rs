//! Discord model types: only the fields tritone needs. Ids are snowflakes,
//! read and written as decimal strings.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Discord's epoch (2015-01-01), in Unix ms.
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

/// A Discord snowflake (guild, channel, user or message id). Serde reads and
/// writes it as a decimal string, like Discord; reading also accepts a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Id(pub u64);

impl Id {
    /// When the id was created, in Unix ms.
    pub fn timestamp_ms(self) -> i64 {
        ((self.0 >> 22) as i64) + DISCORD_EPOCH_MS
    }

    /// An id for a given time, e.g. a nonce. `counter` fills the low bits so
    /// ids made in the same millisecond still differ.
    pub fn from_timestamp_ms(ms: i64, counter: u16) -> Id {
        let since = (ms - DISCORD_EPOCH_MS).max(0) as u64;
        Id((since << 22) | u64::from(counter))
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Id {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Id, Self::Err> {
        s.parse().map(Id)
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Id, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Str(String),
            Num(u64),
        }
        match Raw::deserialize(d)? {
            Raw::Str(s) => s.parse().map_err(serde::de::Error::custom),
            Raw::Num(n) => Ok(Id(n)),
        }
    }
}

impl schemars::JsonSchema for Id {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Id".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "string", "pattern": "^[0-9]+$", "description": "A Discord id (decimal string)" })
    }
}

/// Discord channel types 0, 5, 15, 16, 11, 12, 10, 1, 3, 2, 4; anything else is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    Text,
    Announcement,
    Forum,
    Media,
    PublicThread,
    PrivateThread,
    AnnouncementThread,
    Dm,
    GroupDm,
    Voice,
    Category,
    Other(u8),
}

impl ChannelKind {
    pub fn from_u8(n: u8) -> ChannelKind {
        match n {
            0 => ChannelKind::Text,
            1 => ChannelKind::Dm,
            2 => ChannelKind::Voice,
            3 => ChannelKind::GroupDm,
            4 => ChannelKind::Category,
            5 => ChannelKind::Announcement,
            10 => ChannelKind::AnnouncementThread,
            11 => ChannelKind::PublicThread,
            12 => ChannelKind::PrivateThread,
            15 => ChannelKind::Forum,
            16 => ChannelKind::Media,
            other => ChannelKind::Other(other),
        }
    }

    pub fn is_thread(self) -> bool {
        matches!(self, ChannelKind::PublicThread | ChannelKind::PrivateThread | ChannelKind::AnnouncementThread)
    }

    /// A DM or group DM.
    pub fn is_private(self) -> bool {
        matches!(self, ChannelKind::Dm | ChannelKind::GroupDm)
    }
}

/// A user as tritone shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRef {
    pub id: Id,
    pub username: String,
    pub display_name: Option<String>,
    pub bot: bool,
}

impl UserRef {
    /// The display name, else the username.
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.username)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub id: Id,
    pub permissions: u64,
}

/// A channel permission overwrite: for a role, or for a member when `is_member`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overwrite {
    pub id: Id,
    pub is_member: bool,
    pub allow: u64,
    pub deny: u64,
}

/// `roles` includes @everyone (its id is the guild id). `my_roles` are the
/// user's own roles there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guild {
    pub id: Id,
    pub name: String,
    pub owner_id: Id,
    pub member_count: Option<u32>,
    pub large: bool,
    pub roles: Vec<Role>,
    pub my_roles: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub id: Id,
    pub guild_id: Option<Id>,
    /// The category for a channel, the parent channel for a thread.
    pub parent_id: Option<Id>,
    pub name: Option<String>,
    pub kind: ChannelKind,
    /// DM and group DM members, without the user.
    pub recipients: Vec<UserRef>,
    pub last_message_id: Option<Id>,
    pub overwrites: Vec<Overwrite>,
    pub position: i32,
}

/// Per channel. A channel is unread when its `last_message_id` is past `last_acked`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadState {
    pub last_acked: Option<Id>,
    pub mention_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuteUntil {
    No,
    Forever,
    /// Unix ms.
    Until(i64),
}

impl MuteUntil {
    pub fn active(&self, now_ms: i64) -> bool {
        match self {
            MuteUntil::No => false,
            MuteUntil::Forever => true,
            MuteUntil::Until(t) => *t > now_ms,
        }
    }
}

/// Discord's notification settings for one guild (`guild_id: None` = DMs).
/// `channel_mutes` holds every channel override, including `No` (an explicit unmute).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildSettings {
    pub guild_id: Option<Id>,
    pub muted: MuteUntil,
    pub suppress_everyone: bool,
    pub suppress_roles: bool,
    pub channel_mutes: HashMap<Id, MuteUntil>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: Id,
    pub channel_id: Id,
    pub guild_id: Option<Id>,
    pub author: UserRef,
    pub content: String,
    pub timestamp_ms: i64,
    pub edited: bool,
    /// The message this one replies to, and its author.
    pub reply_to: Option<Id>,
    pub reply_to_author: Option<Id>,
    pub mentions: Vec<Id>,
    pub mention_roles: Vec<Id>,
    pub mention_everyone: bool,
    /// Placeholders such as "image: a.png" and "file: b.pdf".
    pub attachments: Vec<String>,
    /// Embed titles only.
    pub embeds: Vec<String>,
    /// (":name:" or the unicode emoji, count).
    pub reactions: Vec<(String, u32)>,
    /// Discord message type (0 default, 19 reply, ...).
    pub kind: u8,
}

/// What READY carries that tritone keeps. Parse it, slim it, drop the raw JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadySnapshot {
    pub me: UserRef,
    pub session_id: String,
    pub resume_url: String,
    pub guilds: Vec<Guild>,
    /// Guild channels, joined threads, DMs and group DMs.
    pub channels: Vec<Channel>,
    pub read_states: HashMap<Id, ReadState>,
    pub settings: Vec<GuildSettings>,
    pub users: Vec<UserRef>,
}

/// Why a Discord call failed. The Display text goes back to Claude, so it
/// says what the user must do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscordError {
    #[error(
        "the Discord session is not valid (never logged in, or logged out elsewhere): ask the user to run `tritone login` in a terminal"
    )]
    NeedsLogin,
    #[error(
        "Discord asked for a CAPTCHA: open Discord in the official app, do any pending check there, then restart tritone (`pm2 restart tritone`). tritone stopped and will not retry"
    )]
    Captcha,
    #[error(
        "Discord wants the account verified or has locked it: the user must open Discord in the official app and follow its steps. tritone stopped and will not retry"
    )]
    Locked,
    #[error(
        "Discord's edge blocked the request: tritone stopped. Wait at least {retry_after_s} s, and ask the user before trying again"
    )]
    Blocked { retry_after_s: u64 },
    #[error("Discord rate limited this call: wait {retry_after_s} s before trying again")]
    RateLimited { retry_after_s: u64 },
    #[error("Discord's search index for this isn't ready yet: try again in {retry_after_s} s")]
    Indexing { retry_after_s: u64 },
    #[error("Discord says the user has no access to this")]
    Forbidden,
    #[error("Discord says this doesn't exist (deleted, or not visible to the user)")]
    NotFound,
    #[error("Discord returned HTTP {status}{}", code.map(|c| format!(" (code {c})")).unwrap_or_default())]
    Api { status: u16, code: Option<u32> },
    #[error("network error talking to Discord: {0}")]
    Http(String),
}

impl DiscordError {
    /// Errors that stop everything until the user acts: no call retries them.
    pub fn halts(&self) -> bool {
        matches!(
            self,
            DiscordError::NeedsLogin | DiscordError::Captcha | DiscordError::Locked | DiscordError::Blocked { .. }
        )
    }

    /// A label safe for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            DiscordError::NeedsLogin => "needs_login",
            DiscordError::Captcha => "captcha",
            DiscordError::Locked => "locked",
            DiscordError::Blocked { .. } => "blocked",
            DiscordError::RateLimited { .. } => "rate_limited",
            DiscordError::Indexing { .. } => "indexing",
            DiscordError::Forbidden => "forbidden",
            DiscordError::NotFound => "not_found",
            DiscordError::Api { .. } => "api",
            DiscordError::Http(_) => "http",
        }
    }
}

/// A message to send. `nonce` (an id for now) makes a retry never double-post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    pub content: String,
    pub reply_to: Option<Id>,
    pub nonce: Id,
    /// Send without notifying (Discord's @silent).
    pub silent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Guild(Id),
    Channel(Id),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub content: Option<String>,
    pub author_id: Option<Id>,
    pub channel_id: Option<Id>,
    pub min_id: Option<Id>,
    pub max_id: Option<Id>,
    pub offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPage {
    pub total: u32,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuteTarget {
    Guild(Id),
    Channel { guild: Option<Id>, channel: Id },
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
