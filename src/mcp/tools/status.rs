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
            ConnState::Halted(DiscordError::NeedsLogin) => ("needs_login", Some(self.needs_login_note().await)),
            ConnState::Halted(e) => ("halted", Some(e.to_string())),
        };
        let s = Status { connection, user: live.me().map(|u| u.username), guilds: live.guilds().len(), note };
        Ok(json(&s))
    }
}

impl TritoneServer {
    /// While halted on `NeedsLogin`, a token in the keyring means the user
    /// logged in again: wake the gateway instead of asking for a restart.
    async fn needs_login_note(&self) -> String {
        match self.state.secrets.get(ACCOUNT_TOKEN).await {
            Ok(Some(_)) => {
                self.state.login_notify.notify_one();
                "a login was found in the keyring; reconnecting. Call status again in a few seconds.".to_string()
            }
            Ok(None) => DiscordError::NeedsLogin.to_string(),
            Err(e) => format!("could not read the keyring: {e}"),
        }
    }
}
