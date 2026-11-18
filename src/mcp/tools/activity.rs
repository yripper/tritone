//! Contribution log tools: activity summary and backfill.

use rmcp::tool_router;

use crate::mcp::server::TritoneServer;

/// Contribution log tools: activity summary and backfill.
// `allow_empty`: rmcp 3.8 rejects a router with no tools; drop it once the first tool lands.
#[tool_router(router = activity_router, vis = "pub(crate)", allow_empty)]
impl TritoneServer {}
