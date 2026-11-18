//! The MCP server. Tools live in `mcp/tools/`, one router per group, merged
//! here with `+`. Results are pretty JSON text; failures come back as tool
//! errors Claude can read.

use std::future::Future;
use std::path::PathBuf;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool_handler};
use serde::Serialize;

use super::instructions::current_instructions;
use super::state::{AppState, Ctx};
use crate::domain::errors::Result as DResult;

#[derive(Clone)]
pub struct TritoneServer {
    pub(crate) state: AppState,
    style_path: Option<PathBuf>,
    tool_router: ToolRouter<TritoneServer>,
}

/// Pretty JSON as a tool result.
pub(crate) fn json<T: Serialize>(v: &T) -> CallToolResult {
    match serde_json::to_string_pretty(v) {
        Ok(s) => CallToolResult::success(vec![ContentBlock::text(s)]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("serialization failed: {e}"))]),
    }
}

impl TritoneServer {
    /// `style_path` is re-read on every initialize, so editing style.md needs no restart.
    pub fn new(state: AppState, style_path: Option<PathBuf>) -> TritoneServer {
        TritoneServer { state, style_path, tool_router: Self::tool_router() }
    }

    pub fn tool_router() -> ToolRouter<TritoneServer> {
        Self::status_router()
            + Self::read_router()
            + Self::triage_router()
            + Self::write_router()
            + Self::engage_router()
            + Self::activity_router()
    }

    /// Runs one action against the connected context, turning failures into
    /// tool errors.
    pub async fn run<T, F, Fut>(&self, f: F) -> CallToolResult
    where
        T: Serialize,
        F: FnOnce(Ctx) -> Fut,
        Fut: Future<Output = DResult<T>>,
    {
        let outcome = match self.state.ctx() {
            Ok(ctx) => f(ctx).await,
            Err(e) => Err(e),
        };
        match outcome {
            Ok(v) => json(&v),
            Err(e) => {
                // Error text can carry channel and people names: log only the kind.
                tracing::warn!(kind = e.kind(), "tool failed");
                CallToolResult::error(vec![ContentBlock::text(e.to_string())])
            }
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TritoneServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tritone", env!("CARGO_PKG_VERSION")))
            .with_instructions(current_instructions(self.style_path.as_deref()))
    }
}
