//! Engagement tools: cleanup and voice.

use rmcp::tool_router;

use crate::mcp::server::TritoneServer;

/// Engagement tools: cleanup and voice.
// `allow_empty`: rmcp 3.8 rejects a router with no tools; drop it once the first tool lands.
#[tool_router(router = engage_router, vis = "pub(crate)", allow_empty)]
impl TritoneServer {}
