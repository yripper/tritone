//! Contribution log tools: activity summary and backfill.

use std::sync::atomic::{AtomicBool, Ordering};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::domain::activity::{BackfillReport, backfill, backfill_progress, my_activity};
use crate::domain::errors::Error;
use crate::domain::live::ConnState;
use crate::mcp::server::{TritoneServer, json};

/// `kv` prefix of the backfill's per-guild progress.
const BACKFILL_KEY: &str = "backfill";
/// Why the last backfill run stopped, if it didn't finish.
const BACKFILL_ERROR_KEY: &str = "backfill_last_error";
const DAY_MS: i64 = 86_400_000;

/// One backfill at a time: it searches slowly, and two would double the pace.
static BACKFILL_RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MyActivityArgs {
    /// How many days back, ending now. Default 30.
    pub days: Option<u32>,
}

#[derive(Debug, Serialize)]
struct BackfillStatus {
    running: bool,
    #[serde(flatten)]
    progress: BackfillReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    note: &'static str,
}

fn tool_error(e: Error) -> CallToolResult {
    tracing::warn!(kind = e.kind(), "tool failed");
    CallToolResult::error(vec![ContentBlock::text(e.to_string())])
}

#[tool_router(router = activity_router, vis = "pub(crate)")]
impl TritoneServer {
    #[tool(
        description = "What the user posted in servers over the last `days` (default 30): counts by kind, server and channel, and links to the newest 50 messages. Includes messages typed in any Discord app, not only through tritone."
    )]
    async fn my_activity(&self, Parameters(a): Parameters<MyActivityArgs>) -> Result<CallToolResult, McpError> {
        let to = self.state.timer.now_ms();
        let from = to - i64::from(a.days.unwrap_or(30).max(1)) * DAY_MS;
        Ok(match my_activity(&self.state.store, &self.state.live, from, to) {
            Ok(s) => json(&s),
            Err(e) => tool_error(e),
        })
    }

    #[tool(
        description = "Fill the contribution log with the user's older server messages, through Discord search, slowly (one page every few seconds) and in the background. Starts it, or continues where it stopped; call again to see progress. Safe to call repeatedly."
    )]
    async fn backfill_activity(&self) -> Result<CallToolResult, McpError> {
        let ctx = match self.state.ctx() {
            Ok(c) => c,
            Err(e) => return Ok(tool_error(e)),
        };
        // Before READY no guilds are known, and "0 of 0 done" would read as complete.
        if !matches!(ctx.live.connection(), ConnState::Ready { .. }) {
            return Ok(tool_error(Error::NotConnected));
        }
        let progress = match backfill_progress(&ctx.store, &ctx.live, BACKFILL_KEY) {
            Ok(p) => p,
            Err(e) => return Ok(tool_error(e)),
        };
        let finished = progress.guilds_done == progress.guilds_total;
        let started =
            !finished && BACKFILL_RUNNING.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok();
        let last_error = if started {
            // A new run: the previous one's error no longer applies.
            let _ = ctx.store.kv_set(BACKFILL_ERROR_KEY, "");
            None
        } else {
            ctx.store.kv_get(BACKFILL_ERROR_KEY).ok().flatten().filter(|e| !e.is_empty())
        };
        if started {
            tokio::spawn(async move {
                let outcome = backfill(&ctx, BACKFILL_KEY).await;
                if let Err(e) = outcome {
                    let _ = ctx.store.kv_set(BACKFILL_ERROR_KEY, &e.to_string());
                }
                BACKFILL_RUNNING.store(false, Ordering::SeqCst);
            });
        }
        let running = BACKFILL_RUNNING.load(Ordering::SeqCst);
        let note = match (finished, started, running) {
            (true, _, _) => "Backfill is complete for every server the user is in.",
            (_, true, _) => "Backfill started in the background. Call backfill_activity again later for progress.",
            (_, _, true) => "Backfill is running. Call backfill_activity again later for progress.",
            _ => "Backfill stopped before finishing; calling backfill_activity again continues it.",
        };
        Ok(json(&BackfillStatus { running, progress, last_error, note }))
    }
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
