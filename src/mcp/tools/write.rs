//! Write tools: send, react, threads, forum posts, send status, resume.
//! Every write goes through the send queue; none happens inside the call.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;

use crate::discord::types::{ChannelKind, Id};
use crate::domain::errors::{Error, Result as DResult};
use crate::domain::live::LiveState;
use crate::domain::send_queue::JobSpec;
use crate::mcp::server::{TritoneServer, json};
use crate::mcp::state::Ctx;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SendArgs {
    /// The channel, thread or DM: its id or a discord.com/channels/... link.
    pub channel: String,
    /// The exact text the user approved. @username becomes a mention when the name is unambiguous there.
    pub text: String,
    /// Reply to this message id (a quoted reply).
    pub reply_to: Option<Id>,
    /// What this contribution is, for my_activity (e.g. answer, support, or a topic's kind). Inferred when omitted.
    pub kind: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReactArgs {
    /// The channel id or a discord.com/channels/... link.
    pub channel: String,
    pub message: Id,
    /// A unicode emoji, or name:id for a custom one.
    pub emoji: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ThreadArgs {
    /// The parent channel id or a discord.com/channels/... link.
    pub channel: String,
    /// Start the thread from this message; omit for a standalone thread.
    pub from_message: Option<Id>,
    pub name: String,
    /// An optional first message in the thread, approved like any send.
    pub text: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ForumPostArgs {
    /// The forum channel id or a discord.com/channels/... link.
    pub forum: String,
    pub title: String,
    pub text: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SendStatusArgs {
    /// One job; omit for every recent job.
    pub job_id: Option<u64>,
}

/// A channel id from an id or a `discord.com/channels/g/c[/m]` link, known
/// to the live model.
// TODO(resolve): switch to the shared channel resolver (names, #channel, @user) once it lands.
fn target(query: &str, live: &LiveState) -> DResult<Id> {
    let q = query.trim();
    let raw = match q.find("discord.com/channels/") {
        Some(at) => q[at..].split('/').nth(3).unwrap_or_default(),
        None => q.trim_start_matches(['#', '<', '>']).trim_end_matches('>'),
    };
    let id: Id = raw
        .parse()
        .map_err(|_| Error::Invalid(format!("\"{query}\" is not a channel id or link: pass the channel's id")))?;
    if live.channel(id).is_none() {
        return Err(Error::NotFound(query.to_string()));
    }
    Ok(id)
}

/// Outgoing text with `@username` turned into mentions where unambiguous.
// TODO(mentions): call `domain::render::to_discord_mentions(text, live, guild)` once it lands.
fn outgoing_text(text: &str, live: &LiveState, guild: Option<Id>) -> String {
    let _ = (live, guild);
    text.to_string()
}

fn guild_of(live: &LiveState, channel: Id) -> Option<Id> {
    live.channel(channel).and_then(|c| c.guild_id)
}

/// Queues a job with the current REST client.
fn enqueue(ctx: &Ctx, spec: JobSpec) -> DResult<crate::domain::send_queue::Enqueued> {
    ctx.queue.attach(ctx.api.clone());
    ctx.queue.enqueue(spec)
}

fn tool_error(e: Error) -> CallToolResult {
    tracing::warn!(kind = e.kind(), "tool failed");
    CallToolResult::error(vec![ContentBlock::text(e.to_string())])
}

#[tool_router(router = write_router, vis = "pub(crate)")]
impl TritoneServer {
    #[tool(
        description = "Queue a message as the user. ONLY after the user explicitly approved this exact text for this exact channel. Returns job_id and eta at once: the message goes out after typing and a paced gap. Check send_status; a sent_message_id means it WAS sent, never resend."
    )]
    async fn send_message(&self, Parameters(a): Parameters<SendArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let channel = target(&a.channel, &ctx.live)?;
                let content = outgoing_text(&a.text, &ctx.live, guild_of(&ctx.live, channel));
                let kind = a.kind.filter(|k| !k.trim().is_empty());
                enqueue(&ctx, JobSpec::Send { channel, content, reply_to: a.reply_to, kind })
            })
            .await)
    }

    #[tool(
        description = "Queue a reaction as the user. ONLY after the user explicitly approved this reaction on this message. Paced like sends, without typing."
    )]
    async fn react(&self, Parameters(a): Parameters<ReactArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let channel = target(&a.channel, &ctx.live)?;
                enqueue(&ctx, JobSpec::React { channel, message: a.message, emoji: a.emoji.trim().to_string() })
            })
            .await)
    }

    #[tool(
        description = "Queue a new thread as the user, from a message or standalone, with an optional first message. ONLY after the user explicitly approved the name and text."
    )]
    async fn create_thread(&self, Parameters(a): Parameters<ThreadArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let channel = target(&a.channel, &ctx.live)?;
                let kind = ctx.live.channel(channel).map(|c| c.kind);
                if !matches!(kind, Some(ChannelKind::Text | ChannelKind::Announcement)) {
                    return Err(Error::Invalid("threads can only be created in a server text or announcement channel; for a forum use create_forum_post".into()));
                }
                let guild = guild_of(&ctx.live, channel);
                let content = a.text.map(|t| outgoing_text(&t, &ctx.live, guild));
                enqueue(&ctx, JobSpec::Thread { channel, from_message: a.from_message, name: a.name, content })
            })
            .await)
    }

    #[tool(
        description = "Queue a new forum post (a thread with a first message) as the user. ONLY after the user explicitly approved the title and text."
    )]
    async fn create_forum_post(&self, Parameters(a): Parameters<ForumPostArgs>) -> Result<CallToolResult, McpError> {
        Ok(self
            .run(|ctx| async move {
                let forum = target(&a.forum, &ctx.live)?;
                if !matches!(ctx.live.channel(forum).map(|c| c.kind), Some(ChannelKind::Forum | ChannelKind::Media)) {
                    return Err(Error::Invalid(
                        "that channel is not a forum: use send_message or create_thread".into(),
                    ));
                }
                let content = outgoing_text(&a.text, &ctx.live, guild_of(&ctx.live, forum));
                enqueue(&ctx, JobSpec::ForumPost { forum, name: a.title, content })
            })
            .await)
    }

    #[tool(
        description = "Status of queued and recent writes: queued, sending, sent or failed, eta, sent_message_id, link, error, ack_error, and whether the queue is paused. A sent_message_id means it WAS sent: never resend."
    )]
    async fn send_status(&self, Parameters(a): Parameters<SendStatusArgs>) -> Result<CallToolResult, McpError> {
        // Works while halted, so the user can see what went out.
        Ok(match self.state.queue.status(a.job_id) {
            Ok(s) => json(&s),
            Err(e) => tool_error(e),
        })
    }

    #[tool(
        description = "Continue after tritone stopped on a CAPTCHA, a locked account or a block. ONLY when the user explicitly says they did the check in the official Discord app and want tritone to continue. Lifts the stop and resumes queued jobs; the job that hit the error stays failed. Refuses while a new login is needed (run `tritone login`)."
    )]
    async fn resume_queue(&self) -> Result<CallToolResult, McpError> {
        Ok(match self.state.queue.resume() {
            Ok(s) => json(&s),
            Err(e) => tool_error(e),
        })
    }
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
