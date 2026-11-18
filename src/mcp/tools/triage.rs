//! Triage tools: mark read, snooze, watch levels.

use rmcp::tool_router;

use crate::mcp::server::TritoneServer;

/// Triage tools: mark read, snooze, watch levels.
// `allow_empty`: rmcp 3.8 rejects a router with no tools; drop it once the first tool lands.
#[tool_router(router = triage_router, vis = "pub(crate)", allow_empty)]
impl TritoneServer {}
