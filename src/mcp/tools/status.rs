//! The `status` tool: is the Discord connection up, and as whom.

use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Serialize;

use crate::discord::types::DiscordError;
use crate::domain::live::ConnState;
use crate::mcp::server::{TritoneServer, json};
use crate::secrets::ACCOUNT_TOKEN;

#[derive(Debug, Serialize)]
pub(crate) struct Status {
    /// connecting, ready, reconnecting, needs_login or halted.
    pub connection: &'static str,
    /// Username only.
    pub user: Option<String>,
    /// How many servers the account is in.
    pub guilds: usize,
    pub note: Option<String>,
}

#[tool_router(router = status_router, vis = "pub(crate)")]
impl TritoneServer {
    #[tool(description = "Whether tritone is connected to Discord, as whom, and in how many servers.")]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        let live = &self.state.live;
        let (connection, note) = match live.connection() {
            ConnState::Connecting => ("connecting", None),
            ConnState::Ready { .. } => ("ready", None),
            ConnState::Reconnecting => ("reconnecting", Some("the connection dropped; resuming".to_string())),
            ConnState::Halted(DiscordError::NeedsLogin) => self.needs_login().await,
            ConnState::Halted(e) => ("halted", Some(e.to_string())),
        };
        let s = Status { connection, user: live.me().map(|u| u.username), guilds: live.guilds().len(), note };
        Ok(json(&s))
    }
}

impl TritoneServer {
    /// While halted on `NeedsLogin`, a token in the keyring that Discord hasn't
    /// rejected (nor been tried with since the halt) means the user logged in
    /// again: wake the gateway instead of asking for a restart. The dead token
    /// never wakes it, so polling `status` can't drive an identify loop.
    async fn needs_login(&self) -> (&'static str, Option<String>) {
        match self.state.secrets.get(ACCOUNT_TOKEN).await {
            Ok(Some(token)) if self.state.live.try_login_with(&token) => {
                self.state.login_notify.notify_one();
                let note = "a new login was found in the keyring; reconnecting. Call status again in a few seconds.";
                ("connecting", Some(note.to_string()))
            }
            Ok(Some(_)) => {
                let note = format!("Discord rejected the saved login. {}", DiscordError::NeedsLogin);
                ("needs_login", Some(note))
            }
            Ok(None) => ("needs_login", Some(DiscordError::NeedsLogin.to_string())),
            Err(e) => ("needs_login", Some(format!("could not read the keyring: {e}"))),
        }
    }
}
