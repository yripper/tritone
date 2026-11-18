//! The live gateway connection.

use crate::mcp::state::AppState;

/// Starts the gateway task. Stub: returns at once.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        drop(state);
    })
}
