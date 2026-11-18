use crate::discord::types::DiscordError;

/// What a tool call can fail with. The Display text goes back to Claude, so it
/// says what to do next.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Discord(#[from] DiscordError),
    #[error("\"{query}\" matches several: {}. Ask the user which one and pass its id.", candidates.join(", "))]
    Ambiguous { query: String, candidates: Vec<String> },
    #[error(
        "nothing matches \"{0}\". Try an id, a discord.com/channels/... link, list_servers or list_channels, or a shorter part of the name."
    )]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("tritone is not connected to Discord yet. Wait a few seconds and try again; if it persists, call status.")]
    NotConnected,
    #[error("local store error: {0}")]
    Store(#[from] anyhow::Error),
}

impl Error {
    /// A label safe for logs (no names, no message text).
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Discord(e) => e.kind(),
            Error::Ambiguous { .. } => "ambiguous",
            Error::NotFound(_) => "not_found",
            Error::Invalid(_) => "invalid",
            Error::NotConnected => "not_connected",
            Error::Store(_) => "store",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
