//! Write tools: send, react, threads, forum posts, send status.

use rmcp::tool_router;

use crate::mcp::server::TritoneServer;

/// Write tools: send, react, threads, forum posts, send status.
// `allow_empty`: rmcp 3.8 rejects a router with no tools; drop it once the first tool lands.
#[tool_router(router = write_router, vis = "pub(crate)", allow_empty)]
impl TritoneServer {}
