//! Triage tools: mark read, snooze, watch levels.

use chrono::DateTime;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::discord::types::Id;
use crate::domain::errors::{Error, Result as DResult};
use crate::domain::live::LiveState;
use crate::domain::resolve::{resolve_channel, resolve_server, snowflake};
use crate::domain::triage;
use crate::mcp::server::TritoneServer;
use crate::store::{TargetKind, WatchLevel};

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct MarkArgs {
    /// Channels to mark read (ids, links, #name, server/#name, @user). All must resolve, or nothing is marked.
    pub channels: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct SnoozeArgs {
    pub channel: String,
    /// RFC 3339 time with offset, e.g. 2026-11-20T09:00:00-03:00.
    pub until: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct UnsnoozeArgs {
    pub channel: String,
}

#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WatchChoice {
    Full,
    Normal,
    Muted,
    /// Remove tritone's level: the server's level or the default applies again.
    Clear,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct WatchArgs {
    /// A server (id or part of its name) or a channel (id, link, #name, server/#name, @user).
    pub target: String,
    pub level: WatchChoice,
}

/// Channel-shaped queries go to `resolve_channel`; a bare id is either kind;
/// otherwise a server name, then a channel name.
fn resolve_target(q: &str, live: &LiveState) -> DResult<Id> {
    let t = q.trim();
    let channel_like =
        t.starts_with('#') || t.starts_with('@') || t.contains("/#") || t.contains("discord.com/channels/");
    if channel_like {
        return resolve_channel(t, live);
    }
    if let Some(id) = snowflake(t) {
        return Ok(id);
    }
    match resolve_server(t, live) {
        Err(Error::NotFound(_)) => resolve_channel(t, live),
        other => other,
    }
}

fn parse_until(s: &str) -> DResult<i64> {
    DateTime::parse_from_rfc3339(s.trim())
        .map(|t| t.timestamp_millis())
        .map_err(|_| Error::Invalid(format!("\"{s}\" is not an RFC 3339 time: use e.g. 2026-11-20T09:00:00-03:00")))
}

/// Triage tools: mark read, snooze, watch levels.
#[tool_router(router = triage_router, vis = "pub(crate)")]
impl TritoneServer {
    #[tool(
        description = "Mark channels read in Discord up to their last message (bulk, 100 per call). Only with the user's explicit OK."
    )]
    async fn mark_read(&self, Parameters(a): Parameters<MarkArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let ids = a.channels.iter().map(|c| resolve_channel(c, &ctx.live)).collect::<DResult<Vec<Id>>>()?;
                triage::mark_read(&ctx, &ids).await
            })
            .await)
    }

    #[tool(description = "Hide a channel from get_pending until a time. It stays unread in Discord.")]
    async fn snooze(&self, Parameters(a): Parameters<SnoozeArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let channel = resolve_channel(&a.channel, &ctx.live)?;
                triage::snooze(&ctx, channel, parse_until(&a.until)?)?;
                Ok(json!({ "channel": channel, "snoozed_until": a.until.trim() }))
            })
            .await)
    }

    #[tool(description = "Undo snooze.")]
    async fn unsnooze(&self, Parameters(a): Parameters<UnsnoozeArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let channel = resolve_channel(&a.channel, &ctx.live)?;
                triage::unsnooze(&ctx, channel)?;
                Ok(json!({ "channel": channel, "snoozed": false }))
            })
            .await)
    }

    #[tool(
        description = "Set tritone's watch level for a server or channel: full (any unread is pending), normal (only mentions and replies), muted (never pending, hidden), or clear. A channel's level beats its server's; an explicit level beats a Discord mute. Never change several without asking."
    )]
    async fn set_watch_level(&self, Parameters(a): Parameters<WatchArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let target = resolve_target(&a.target, &ctx.live)?;
                let level = match a.level {
                    WatchChoice::Full => Some(WatchLevel::Full),
                    WatchChoice::Normal => Some(WatchLevel::Normal),
                    WatchChoice::Muted => Some(WatchLevel::Muted),
                    WatchChoice::Clear => None,
                };
                let kind = match triage::set_watch(&ctx, target, level)? {
                    TargetKind::Guild => "server",
                    TargetKind::Channel => "channel",
                };
                Ok(json!({ "target": target, "kind": kind, "level": level }))
            })
            .await)
    }
}
