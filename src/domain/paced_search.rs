//! Discord search for voice samples and cleanup, paced like a person: one
//! search at a time, a random 3–6 s between requests, waiting out an index
//! that isn't ready or a rate limit, and stopping on any halt.
//! https://docs.discord.food/resources/message#search-guild-messages

use std::sync::{Arc, LazyLock};

use tokio::sync::Mutex;

use crate::discord::types::{DiscordError, SearchPage, SearchQuery, SearchScope};
use crate::domain::clock::{GapRng, ThreadRng};
use crate::domain::errors::Result;
use crate::domain::live::ConnState;
use crate::domain::send_queue::halt_everything;
use crate::mcp::state::Ctx;

/// Random wait between two searches.
pub const SEARCH_GAP_MIN_MS: u64 = 3_000;
pub const SEARCH_GAP_MAX_MS: u64 = 6_000;
/// Waits for an index or a rate limit before one search gives up.
pub const MAX_WAITS: u32 = 3;
/// Used when Discord says to retry after 0 s.
const DEFAULT_RETRY_S: u64 = 5;

pub struct SearchPacer {
    rng: Arc<dyn GapRng>,
    /// When the last search finished (Unix ms). Held across the request, so
    /// searches from concurrent tool calls queue up one at a time.
    last: Mutex<Option<i64>>,
}

static GLOBAL: LazyLock<SearchPacer> = LazyLock::new(|| SearchPacer::new(Arc::new(ThreadRng)));

impl SearchPacer {
    pub fn new(rng: Arc<dyn GapRng>) -> SearchPacer {
        SearchPacer { rng, last: Mutex::new(None) }
    }

    /// The one pacer the MCP tools share.
    pub fn global() -> &'static SearchPacer {
        &GLOBAL
    }

    /// One search, after the pacing gap. `Indexing` and `RateLimited` are
    /// waited out up to `MAX_WAITS` times; a halting error halts tritone; a
    /// halt set elsewhere refuses before any request.
    pub async fn search(&self, ctx: &Ctx, scope: SearchScope, query: &SearchQuery) -> Result<SearchPage> {
        let mut last = self.last.lock().await;
        let mut waits = 0;
        loop {
            if let ConnState::Halted(e) = ctx.live.connection() {
                return Err(e.into());
            }
            let now = ctx.timer.now_ms();
            // A last time ahead of now means another clock (a test, a clock
            // change): don't wait on it.
            if let Some(l) = (*last).filter(|l| *l <= now) {
                let gap = self.rng.gap_ms(SEARCH_GAP_MIN_MS, SEARCH_GAP_MAX_MS) as i64;
                ctx.timer.sleep_until(l + gap).await;
            }
            let outcome = ctx.api.search(scope, query).await;
            *last = Some(ctx.timer.now_ms());
            match outcome {
                Ok(page) => return Ok(page),
                Err(DiscordError::Indexing { retry_after_s } | DiscordError::RateLimited { retry_after_s })
                    if waits < MAX_WAITS =>
                {
                    waits += 1;
                    let wait_s = if retry_after_s == 0 { DEFAULT_RETRY_S } else { retry_after_s };
                    ctx.timer.sleep_until(ctx.timer.now_ms() + wait_s as i64 * 1000).await;
                }
                Err(e) => {
                    if e.halts() {
                        halt_everything(&ctx.live, &e);
                    }
                    tracing::warn!(kind = e.kind(), "search failed");
                    return Err(e.into());
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "paced_search_tests.rs"]
mod tests;
