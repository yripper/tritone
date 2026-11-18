//! Discord model types. Ids are snowflakes, read and written as decimal strings.

/// A Discord snowflake (guild, channel, user or message id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub u64);

/// A user as tritone shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRef {
    pub id: Id,
    pub username: String,
    pub display_name: Option<String>,
    pub bot: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub id: Id,
    pub permissions: u64,
}

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

/// Why a Discord call failed. The Display text goes back to Claude, so it
/// says what the user must do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscordError {
    #[error(
        "the Discord session is not valid (never logged in, or logged out elsewhere): ask the user to run `tritone login` in a terminal, then try again"
    )]
    NeedsLogin,
    #[error(
        "Discord asked for a CAPTCHA: open Discord in the official app, do any pending check there, then tell Claude to continue. tritone stopped and will not retry"
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
