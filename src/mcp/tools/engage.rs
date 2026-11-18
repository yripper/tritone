//! Engagement tools: opportunities, voice and cleanup.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::discord::types::Id;
use crate::domain::errors::{Error, Result as DResult};
use crate::domain::live::LiveState;
use crate::domain::paced_search::SearchPacer;
use crate::domain::resolve::{resolve_channel, resolve_server};
use crate::domain::{cleanup, opportunities, voice};
use crate::mcp::server::{TritoneServer, json};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ChannelArgs {
    /// The channel, thread or DM: an id, a discord.com/channels/... link, #name, server/#name, @user or part of a name.
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
    /// The channel, thread or DM: an id, a discord.com/channels/... link, #name, server/#name, @user or part of a name.
    pub channel: String,
    /// How the user writes there (up to 2,000 characters).
    pub body: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CategoryArgs {
    /// A channel, thread or DM (as for get_voice), or a server by id or name.
    pub target: String,
    /// A slug like dev-community, work, friends or general.
    pub category: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ApplyArgs {
    /// Item ids from cleanup_plan, each one approved by the user: leave:<server>, mute:<server>, mute:<server>/<channel> or read:<server>@<newest>.
    pub items: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct OpportunitiesArgs {
    /// How many to return, newest first. Default 15, at most 50.
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DismissArgs {
    /// The opportunity's id (its message id) from get_opportunities.
    pub id: Id,
}

#[derive(Serialize)]
struct Found {
    opportunities: Vec<opportunities::Opportunity>,
}

#[derive(Serialize)]
struct Applied {
    results: Vec<cleanup::ApplyResult>,
}

#[derive(Serialize)]
struct Saved {
    ok: bool,
}

#[derive(Serialize)]
struct Categories {
    categories: Vec<String>,
}

/// `<#id>` as Discord writes a channel mention, else the query itself.
fn unwrap_mention(query: &str) -> &str {
    let q = query.trim();
    q.strip_prefix("<#").and_then(|r| r.strip_suffix('>')).unwrap_or(q)
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

/// A channel the live model knows, by id, link, `#name`, `server/#name`,
/// `@user` or part of a name.
fn channel_arg(query: &str, live: &LiveState) -> DResult<Id> {
    let id = resolve_channel(unwrap_mention(query), live)?;
    live.channel(id).map(|_| id).ok_or_else(|| Error::NotFound(query.to_string()))
}

/// A channel (as `channel_arg`), else a server by id or name.
fn target_arg(query: &str, live: &LiveState) -> DResult<Id> {
    match channel_arg(query, live) {
        Err(Error::NotFound(_)) => resolve_server(query, live),
        other => other,
    }
}

#[tool_router(router = engage_router, vis = "pub(crate)")]
impl TritoneServer {
    #[tool(
        description = "Places in full-watch server channels where the user could contribute, newest first: unanswered questions (shown once 30 min old, until 48 h), messages matching the user's configured topics, and new messages in threads the user wrote in or replies to them. Each has the message, up to 5 messages before it, the channel, its age in minutes, the rule, a link, and kind (pass it to send_message when answering). Ones someone else already answered are dropped. Doesn't mark anything read."
    )]
    async fn get_opportunities(
        &self,
        Parameters(a): Parameters<OpportunitiesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = a.limit.unwrap_or(15).clamp(1, 50);
        Ok(self
            .run(|ctx| async move {
                opportunities::get_opportunities(&ctx, limit).await.map(|opportunities| Found { opportunities })
            })
            .await)
    }

    #[tool(description = "Hide one opportunity for good (it never comes back, even after a restart).")]
    async fn dismiss_opportunity(&self, Parameters(a): Parameters<DismissArgs>) -> Result<CallToolResult, McpError> {
        let now = self.state.timer.now_ms();
        let done = opportunities::dismiss_opportunity(&self.state.store, a.id, now);
        Ok(local(done.map(|_| Saved { ok: true })))
    }

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

    #[tool(
        description = "Propose cleanup, changing nothing: every server with its unread channels, whether it's muted in Discord, when the user last posted there, whether it has full-watch channels, and a suggestion (leave after 180 days without a post, unless something there is on full watch or the user owns it; mute with more than 20 unread channels and no post in 60 days). Each server lists item ids to pass to cleanup_apply. The last post comes from Discord search, a few servers per call and a few seconds apart, cached for a day: when lookups_pending is above 0, call it again."
    )]
    async fn cleanup_plan(&self) -> Result<CallToolResult, McpError> {
        Ok(self.run(|ctx| async move { cleanup::cleanup_plan(&ctx).await }).await)
    }

    #[tool(
        description = "Carry out cleanup items from cleanup_plan, exactly as given, one at a time and a few seconds apart: leave:<server> leaves it, mute:<server> or mute:<server>/<channel> mutes it in Discord, read:<server>@<newest> marks its unread channels read, but never past the plan's newest message: channels with newer messages stay unread and come back as unseen, to show the user. Leaving and muting can't be undone from here: pass only items the user explicitly approved, each one on its own, never a whole list on a general OK. Stops at a CAPTCHA, a lock or a block."
    )]
    async fn cleanup_apply(&self, Parameters(a): Parameters<ApplyArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move { cleanup::cleanup_apply(&ctx, &a.items).await.map(|results| Applied { results }) })
            .await)
    }
}

#[cfg(test)]
#[path = "engage_tests.rs"]
mod tests;
