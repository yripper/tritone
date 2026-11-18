//! Engagement tools: voice and cleanup.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::discord::types::Id;
use crate::domain::errors::{Error, Result as DResult};
use crate::domain::live::LiveState;
use crate::domain::paced_search::SearchPacer;
use crate::domain::voice;
use crate::mcp::server::{TritoneServer, json};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ChannelArgs {
    /// The channel, thread or DM: its id or a discord.com/channels/... link.
    pub channel: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProfileArgs {
    /// A slug like dev-community, work, friends or general.
    pub category: String,
    /// The style description (up to 4,000 characters).
    pub body: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct NotesArgs {
    /// The channel, thread or DM: its id or a discord.com/channels/... link.
    pub channel: String,
    /// How the user writes there (up to 2,000 characters).
    pub body: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CategoryArgs {
    /// A channel, thread or DM (id or discord.com/channels/... link), or a server id.
    pub target: String,
    /// A slug like dev-community, work, friends or general.
    pub category: String,
}

#[derive(Serialize)]
struct Saved {
    ok: bool,
}

#[derive(Serialize)]
struct Categories {
    categories: Vec<String>,
}

/// The id in an id, `<#id>` or a `discord.com/channels/g/c[/m]` link (the
/// channel part; a server-only link gives the server).
fn id_in(query: &str) -> DResult<Id> {
    let q = query.trim();
    let raw = match q.find("discord.com/channels/") {
        Some(at) => {
            let parts: Vec<&str> = q[at..].split('/').collect();
            parts.get(3).filter(|p| !p.is_empty()).or(parts.get(2)).copied().unwrap_or_default()
        }
        None => q.trim_start_matches(['#', '<', '>']).trim_end_matches('>'),
    };
    raw.parse().map_err(|_| Error::Invalid(format!("\"{query}\" is not an id or a discord.com/channels/... link")))
}

/// A tool result for work that needs no Discord connection (the store and
/// the live model only).
fn local<T: Serialize>(outcome: DResult<T>) -> CallToolResult {
    match outcome {
        Ok(v) => json(&v),
        Err(e) => {
            tracing::warn!(kind = e.kind(), "tool failed");
            CallToolResult::error(vec![ContentBlock::text(e.to_string())])
        }
    }
}

/// A channel the live model knows.
// TODO(resolve): switch to the shared channel resolver (names, #channel, @user) once it lands.
fn channel_arg(query: &str, live: &LiveState) -> DResult<Id> {
    let id = id_in(query)?;
    live.channel(id).map(|_| id).ok_or_else(|| Error::NotFound(query.to_string()))
}

/// A channel or a server the live model knows.
fn target_arg(query: &str, live: &LiveState) -> DResult<Id> {
    let id = id_in(query)?;
    if live.channel(id).is_some() || live.guild(id).is_some() {
        return Ok(id);
    }
    Err(Error::NotFound(query.to_string()))
}

#[tool_router(router = engage_router, vis = "pub(crate)")]
impl TritoneServer {
    #[tool(
        description = "Call before drafting for a channel. Returns its category (its own, else its parent channel's or server's, else general), the saved tone profile and channel notes, and profile_stale, notes_stale (also true with no notes) and needs_learning. When learning is needed it also returns the user's own recent messages there (my_messages_in_channel, up to 60) and, when needs_learning, from up to 3 other channels or servers of the category (my_messages_in_category, up to 40), fetched live through Discord search, slowly (a few seconds per page). Messages sent through tritone are left out."
    )]
    async fn get_voice(&self, Parameters(a): Parameters<ChannelArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let channel = channel_arg(&a.channel, &ctx.live)?;
                voice::get_voice(&ctx, SearchPacer::global(), channel).await
            })
            .await)
    }

    #[tool(
        description = "Save the tone profile for a category (replaces the previous one). Describe the style; do not copy private facts verbatim."
    )]
    async fn save_profile(&self, Parameters(a): Parameters<ProfileArgs>) -> Result<CallToolResult, McpError> {
        let now = self.state.timer.now_ms();
        Ok(local(voice::save_profile(&self.state.store, &a.category, &a.body, now).map(|_| Saved { ok: true })))
    }

    #[tool(
        description = "Save notes about how the user writes in one channel, thread or DM (replaces the previous ones). Describe the style; do not copy private facts verbatim."
    )]
    async fn save_chat_notes(&self, Parameters(a): Parameters<NotesArgs>) -> Result<CallToolResult, McpError> {
        let now = self.state.timer.now_ms();
        let saved = channel_arg(&a.channel, &self.state.live)
            .and_then(|channel| voice::save_chat_notes(&self.state.store, channel, &a.body, now));
        Ok(local(saved.map(|_| Saved { ok: true })))
    }

    #[tool(
        description = "Set the category of a channel or a whole server (a slug like dev-community, work, friends or general). A channel's own category beats its server's. Only after the user agrees."
    )]
    async fn set_category(&self, Parameters(a): Parameters<CategoryArgs>) -> Result<CallToolResult, McpError> {
        let saved = target_arg(&a.target, &self.state.live)
            .and_then(|target| voice::set_category(&self.state.store, target, &a.category));
        Ok(local(saved.map(|_| Saved { ok: true })))
    }

    #[tool(description = "Categories whose saved tone profile is due for a refresh.")]
    async fn list_stale_profiles(&self) -> Result<CallToolResult, McpError> {
        let now = self.state.timer.now_ms();
        let stale = voice::stale_profiles(&self.state.store, &self.state.live, now);
        Ok(local(stale.map(|categories| Categories { categories })))
    }
}

#[cfg(test)]
#[path = "engage_tests.rs"]
mod tests;
