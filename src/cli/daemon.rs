//! `tritone start`: the long-running MCP server pm2 keeps alive.

use std::sync::Arc;

use anyhow::{Context, Result};
use rand::RngCore;

use crate::config::{Config, Paths};
use crate::discord::gateway;
use crate::domain::clock::{SystemTimer, ThreadRng, Timer};
use crate::domain::live::{BufferPolicy, LiveState};
use crate::domain::send_queue::SendQueue;
use crate::mcp::http::serve;
use crate::mcp::server::TritoneServer;
use crate::mcp::state::AppState;
use crate::secrets::{ACCOUNT_BEARER, SecretStore, default_store};
use crate::store::Store;

/// Hex, not base64: a token starting with '-' could be read as a flag inside
/// the `security -i` command line.
pub async fn ensure_bearer_token(secrets: &dyn SecretStore) -> Result<String> {
    if let Some(t) = secrets.get(ACCOUNT_BEARER).await? {
        return Ok(t);
    }
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    secrets.set(ACCOUNT_BEARER, &token).await?;
    Ok(token)
}

pub fn buffer_policy(config: &Config) -> BufferPolicy {
    BufferPolicy {
        per_channel: config.recent_per_channel,
        max_age_ms: i64::from(config.recent_max_age_hours) * 3_600_000,
    }
}

pub async fn start() -> Result<()> {
    let paths = Paths::resolve()?;
    let config = Arc::new(Config::load(&paths)?);
    // Binding first doubles as the single-instance guard: a second daemon
    // must never open a second gateway session.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.port))
        .await
        .with_context(|| format!("port {} is busy: tritone is probably already running (pm2 status)", config.port))?;
    let secrets: Arc<dyn SecretStore> = Arc::from(default_store()?);
    let token = ensure_bearer_token(secrets.as_ref()).await?;
    let store = Arc::new(Store::open(&paths.db)?);

    let timer: Arc<dyn Timer> = Arc::new(SystemTimer);
    let live = Arc::new(LiveState::new(buffer_policy(&config)));
    let queue = Arc::new(SendQueue::new(store.clone(), timer.clone(), Arc::new(ThreadRng)));
    let state = AppState::new(store, secrets, live, queue.clone(), config.clone(), timer);

    gateway::spawn(state.clone());
    tokio::spawn(queue.run());
    tracing::info!(port = config.port, "tritone listening on 127.0.0.1");
    serve(TritoneServer::new(state, Some(paths.style)), listener, token).await
}
