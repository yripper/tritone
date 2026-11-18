//! Read tools: servers, channels, pending, history, threads, search, channel info, people.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::discord::types::{Id, SearchQuery, SearchScope};
use crate::domain::errors::{Error, Result as DResult};
use crate::domain::paced_search::SearchPacer;
use crate::domain::pending::get_pending;
use crate::domain::reading::{self, KindFilter};
use crate::domain::resolve::{resolve_channel, resolve_server, resolve_user, snowflake};
use crate::mcp::server::TritoneServer;
use crate::store::WatchLevel;

/// `list_channels` returns at most this many.
const MAX_CHANNELS: usize = 300;
/// Discord's search refuses offsets past this.
const MAX_SEARCH_OFFSET: u32 = 9_975;

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct ListServersArgs {
    /// Also list servers muted in tritone (default false).
    pub include_muted: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct ListChannelsArgs {
    /// Only this kind: dm, group_dm, server (server channels) or thread.
    pub kind: Option<KindFilter>,
    /// Only this server: its id or part of its name.
    pub server: Option<String>,
    /// Only channels at this watch level. Muted channels show only when asked for.
    pub watch: Option<WatchLevel>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct HistoryArgs {
    /// Channel id, discord.com/channels/... link, #name, server/#name, @user (DM) or part of a name.
    pub channel: String,
    /// How many messages, newest first from Discord (1-100, default 50).
    pub limit: Option<u32>,
    /// Only messages older than this message id (for paging back). Never marks read.
    pub before: Option<String>,
    /// Read without marking read (default false).
    pub peek: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct ThreadArgs {
    /// The thread's id or link (a forum post is a thread too).
    pub thread: String,
    /// 1-100, default 50.
    pub limit: Option<u32>,
    /// Read without marking read (default false).
    pub peek: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct SearchArgs {
    /// Words to look for (may be empty to list by author).
    pub query: String,
    /// The server to search: id or part of its name.
    pub server: Option<String>,
    /// The channel or DM to search (as in read_history). Wins over `server`.
    pub channel: Option<String>,
    /// Only messages by this person: id, username or display name.
    pub author: Option<String>,
    /// Skip this many results (pages are 25).
    pub offset: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct ChannelArgs {
    /// Channel id, discord.com/channels/... link, #name, server/#name, @user (DM) or part of a name.
    pub channel: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub(crate) struct FindUserArgs {
    /// Part of a username or display name.
    pub query: String,
}

/// 1..=100, `default` when not given.
fn limit_of(limit: Option<u32>, default: u8) -> u8 {
    limit.map_or(default, |l| l.clamp(1, 100) as u8)
}

fn message_id(s: &str) -> DResult<Id> {
    snowflake(s.trim()).ok_or_else(|| Error::Invalid(format!("\"{s}\" is not a message id")))
}

/// Read tools: servers, channels, pending, history, threads, search, channel info, people.
#[tool_router(router = read_router, vis = "pub(crate)")]
impl TritoneServer {
    #[tool(
        description = "Servers the user is in, with unread channel counts and mutes. Servers muted in tritone are hidden unless include_muted."
    )]
    async fn list_servers(&self, Parameters(a): Parameters<ListServersArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let servers =
                    reading::list_servers(&ctx.live, &ctx.store, ctx.timer.now_ms(), a.include_muted.unwrap_or(false))?;
                Ok(json!({ "servers": servers }))
            })
            .await)
    }

    #[tool(
        description = "Channels, DMs, group DMs and joined threads the user can see, with watch level, unread state and mutes. Muted ones are hidden unless watch=\"muted\"."
    )]
    async fn list_channels(&self, Parameters(a): Parameters<ListChannelsArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let server = a.server.as_deref().map(|s| resolve_server(s, &ctx.live)).transpose()?;
                let mut channels =
                    reading::list_channels(&ctx.live, &ctx.store, a.kind, server, a.watch, ctx.timer.now_ms())?;
                let truncated = channels.len() > MAX_CHANNELS;
                channels.truncate(MAX_CHANNELS);
                Ok(json!({ "channels": channels, "truncated": truncated }))
            })
            .await)
    }

    #[tool(
        description = "What's waiting on the user: unread DMs and group DMs, mentions and replies, full-watch channels and joined threads, each with its first unread messages (up to 20) and more_unread. truncated: the mentions inbox was too long to read whole. Does not mark anything read."
    )]
    async fn get_pending(&self) -> Result<CallToolResult, McpError> {
        Ok(self.run(|ctx| async move { get_pending(&ctx).await }).await)
    }

    #[tool(
        description = "Recent messages of a channel or DM, oldest first. Marks the channel read up to the newest message returned, unless peek: true or before is given."
    )]
    async fn read_history(&self, Parameters(a): Parameters<HistoryArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let channel = resolve_channel(&a.channel, &ctx.live)?;
                let before = a.before.as_deref().map(message_id).transpose()?;
                reading::read_history(&ctx, channel, limit_of(a.limit, 50), before, a.peek.unwrap_or(false)).await
            })
            .await)
    }

    #[tool(description = "Messages of a thread or forum post, oldest first. Marks it read unless peek: true.")]
    async fn read_thread(&self, Parameters(a): Parameters<ThreadArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let thread = resolve_channel(&a.thread, &ctx.live)?;
                reading::read_history(&ctx, thread, limit_of(a.limit, 50), None, a.peek.unwrap_or(false)).await
            })
            .await)
    }

    #[tool(
        description = "Search messages with Discord's own search, in one server or one channel/DM. Newest first, 25 per page; use offset for more."
    )]
    async fn search_messages(&self, Parameters(a): Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let author_id = a.author.as_deref().map(|u| resolve_user(u, &ctx.live)).transpose()?;
                let content = Some(a.query.trim().to_string()).filter(|q| !q.is_empty());
                let offset = a.offset.unwrap_or(0).min(MAX_SEARCH_OFFSET);
                let mut query = SearchQuery { content, author_id, offset, ..Default::default() };
                let scope = match (a.channel, a.server) {
                    (Some(c), _) => {
                        let c = resolve_channel(&c, &ctx.live)?;
                        // Server channels are searched through their server.
                        match ctx.live.channel(c).and_then(|ch| ch.guild_id) {
                            Some(g) => {
                                query.channel_id = Some(c);
                                SearchScope::Guild(g)
                            }
                            None => SearchScope::Channel(c),
                        }
                    }
                    (None, Some(s)) => SearchScope::Guild(resolve_server(&s, &ctx.live)?),
                    (None, None) => {
                        return Err(Error::Invalid(
                            "search needs a server or a channel: pass server or channel".into(),
                        ));
                    }
                };
                reading::search(&ctx, SearchPacer::global(), scope, &query).await
            })
            .await)
    }

    #[tool(
        description = "One channel's details: kind, watch level (effective and explicit), Discord mute, unread, mention count, snooze, last message time."
    )]
    async fn get_channel_info(&self, Parameters(a): Parameters<ChannelArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let channel = resolve_channel(&a.channel, &ctx.live)?;
                reading::channel_info(&ctx.live, &ctx.store, channel, ctx.timer.now_ms())
            })
            .await)
    }

    #[tool(
        description = "Find people tritone knows (friends, DM people, recent authors) by part of their name, to get their id for <@USER_ID> mentions or their DM."
    )]
    async fn find_user(&self, Parameters(a): Parameters<FindUserArgs>) -> Result<CallToolResult, McpError> {
        Ok(self.run(|ctx| async move { Ok(json!({ "users": reading::find_user(&ctx.live, &a.query) })) }).await)
    }
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
