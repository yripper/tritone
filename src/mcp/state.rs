//! State shared by every MCP request and the background tasks.
//!
//! The gateway task builds the REST client once it has a token and stores it
//! in `api`. Tools reach Discord only through `ctx()`, which refuses while the
//! connection is halted (e.g. the token went bad), so nothing retries in a loop.
//!
//! Halt contract (`LiveState`): a dead token (gateway close 4004, REST 401)
//! calls `live.reject_token(token)`; any other halting error calls
//! `live.halt(e)`. Gateway reconnects never lift a halt. `status` lifts a
//! `NeedsLogin` halt only for a keyring token that differs from the rejected
//! one, then wakes the gateway through `login_notify`.

use std::sync::{Arc, RwLock};

use tokio::sync::Notify;

use crate::config::Config;
use crate::discord::DiscordApi;
use crate::domain::clock::Timer;
use crate::domain::errors::{Error, Result};
use crate::domain::live::{ConnState, LiveState};
use crate::domain::send_queue::SendQueue;
use crate::secrets::SecretStore;
use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub secrets: Arc<dyn SecretStore>,
    pub live: Arc<LiveState>,
    /// Set by the gateway once it has a token. `std` lock: never held across an await.
    pub api: Arc<RwLock<Option<Arc<dyn DiscordApi>>>>,
    pub queue: Arc<SendQueue>,
    pub config: Arc<Config>,
    pub timer: Arc<dyn Timer>,
    /// Wakes a gateway halted with `NeedsLogin` after a new login. Notified
    /// only after `LiveState::try_login_with` accepted the keyring's token.
    pub login_notify: Arc<Notify>,
}

/// What a tool needs to do its work, once connected.
#[derive(Clone)]
pub struct Ctx {
    pub store: Arc<Store>,
    pub live: Arc<LiveState>,
    pub api: Arc<dyn DiscordApi>,
    pub queue: Arc<SendQueue>,
    pub config: Arc<Config>,
    pub timer: Arc<dyn Timer>,
}

impl AppState {
    /// Starts with no REST client: the gateway sets it.
    pub fn new(
        store: Arc<Store>,
        secrets: Arc<dyn SecretStore>,
        live: Arc<LiveState>,
        queue: Arc<SendQueue>,
        config: Arc<Config>,
        timer: Arc<dyn Timer>,
    ) -> AppState {
        AppState {
            store,
            secrets,
            live,
            api: Arc::new(RwLock::new(None)),
            queue,
            config,
            timer,
            login_notify: Arc::new(Notify::new()),
        }
    }

    pub fn set_api(&self, api: Option<Arc<dyn DiscordApi>>) {
        *self.api.write().unwrap_or_else(|e| e.into_inner()) = api;
    }

    /// The connected context. A halted connection returns its reason (e.g.
    /// `NeedsLogin`, whose text says to run `tritone login`); no client yet
    /// means `NotConnected`.
    pub fn ctx(&self) -> Result<Ctx> {
        if let ConnState::Halted(e) = self.live.connection() {
            return Err(Error::Discord(e));
        }
        let api = self.api.read().unwrap_or_else(|e| e.into_inner()).clone().ok_or(Error::NotConnected)?;
        Ok(Ctx {
            store: self.store.clone(),
            live: self.live.clone(),
            api,
            queue: self.queue.clone(),
            config: self.config.clone(),
            timer: self.timer.clone(),
        })
    }
}
