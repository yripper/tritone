//! The in-memory model of the account, fed by the gateway.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use crate::discord::types::{DiscordError, Guild, Id, UserRef};

/// How many recent messages to keep per buffered channel, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferPolicy {
    pub per_channel: usize,
    pub max_age_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Ready { since_ms: i64 },
    Reconnecting,
    Halted(DiscordError),
}

#[derive(Default)]
struct Inner {
    conn: Option<ConnState>,
    me: Option<UserRef>,
    guilds: HashMap<Id, Guild>,
}

pub struct LiveState {
    policy: BufferPolicy,
    inner: Mutex<Inner>,
}

impl LiveState {
    pub fn new(policy: BufferPolicy) -> LiveState {
        LiveState { policy, inner: Mutex::new(Inner::default()) }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn policy(&self) -> BufferPolicy {
        self.policy
    }

    pub fn connection(&self) -> ConnState {
        self.lock().conn.clone().unwrap_or(ConnState::Connecting)
    }

    pub fn me(&self) -> Option<UserRef> {
        self.lock().me.clone()
    }

    pub fn guilds(&self) -> Vec<Guild> {
        self.lock().guilds.values().cloned().collect()
    }
}
