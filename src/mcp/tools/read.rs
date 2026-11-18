//! Read tools: servers, channels, pending, opportunities, history, threads, search, users.

use rmcp::tool_router;

use crate::mcp::server::TritoneServer;

/// Read tools: servers, channels, pending, opportunities, history, threads, search, users.
// `allow_empty`: rmcp 3.8 rejects a router with no tools; drop it once the first tool lands.
#[tool_router(router = read_router, vis = "pub(crate)", allow_empty)]
impl TritoneServer {}
