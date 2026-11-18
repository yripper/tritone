//! The live gateway connection: one persistent socket, like the web client,
//! feeding `LiveState`. Also builds (and rebuilds on a new login) the REST
//! client tools use.

mod conn;
mod events;
mod objects;
mod ready;
mod subscribe;
mod zlib;

use std::sync::Arc;

use async_trait::async_trait;

pub use events::GatewayEvent;

use self::conn::{GATEWAY_URL, Gateway, Opener};
use crate::discord::DiscordApi;
use crate::discord::fingerprint::Fingerprint;
use crate::discord::http::rebuild_api;
use crate::discord::transport::{CookieJar, http_client};
use crate::domain::live::LiveState;
use crate::mcp::state::AppState;
use crate::secrets::SecretStore;

/// Reloads the daemon's one cookie jar from the keyring (a login may have
/// replaced it) and builds a fresh REST client and HTTP client for a token.
struct HttpOpener {
    jar: Arc<CookieJar>,
    secrets: Arc<dyn SecretStore>,
    fp: Arc<Fingerprint>,
    live: Arc<LiveState>,
}

#[async_trait]
impl Opener for HttpOpener {
    async fn open(&self, token: &str) -> anyhow::Result<(Arc<dyn DiscordApi>, wreq::Client)> {
        rebuild_api(&self.jar, self.secrets.as_ref(), self.fp.clone(), self.live.clone(), token.to_string()).await
    }
}

/// Starts the gateway task for the daemon's lifetime. It owns the one cookie
/// jar (and its saver) and the fingerprint, refreshed once at start.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let jar = match CookieJar::load(state.secrets.as_ref()).await {
            Ok(jar) => Arc::new(jar),
            Err(e) => {
                tracing::warn!("reading the cookie jar failed; starting empty: {e:#}");
                Arc::new(CookieJar::empty())
            }
        };
        jar.clone().spawn_saver(state.secrets.clone(), state.timer.clone());
        let mut fp = Fingerprint::load(&state.store, state.secrets.as_ref()).unwrap_or_else(|e| {
            tracing::warn!("reading the cached fingerprint failed: {e:#}");
            Fingerprint::for_this_machine()
        });
        match http_client(jar.clone()) {
            Ok(client) => fp.refresh(&client, &state.store, state.timer.now_ms()).await,
            Err(e) => tracing::warn!("building the HTTP client failed: {e:#}"),
        }
        let fp = Arc::new(fp);
        let opener =
            Arc::new(HttpOpener { jar, secrets: state.secrets.clone(), fp: fp.clone(), live: state.live.clone() });
        Gateway::new(state, opener, fp, GATEWAY_URL.to_string()).run().await;
    })
}
