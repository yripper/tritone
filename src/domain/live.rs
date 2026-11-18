//! The in-memory model of the account, fed by the gateway: guilds, channels,
//! read states, mutes, users, and a bounded buffer of recent messages.
//!
//! Message text lives only in that buffer, for channels the caller marks as
//! buffered (full watch) and for DMs, and is never written to disk.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, MutexGuard};

use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{
    Channel, ChannelKind, DiscordError, Guild, GuildSettings, Id, Message, ReadState, ReadySnapshot, UserRef,
};

/// VIEW_CHANNEL.
const VIEW_CHANNEL: u64 = 1 << 10;
/// ADMINISTRATOR.
const ADMINISTRATOR: u64 = 1 << 3;
/// Live events are dropped for receivers that lag further than this.
const EVENT_CAPACITY: usize = 1024;

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

/// Published to subscribers as the model changes.
// Nearly every event is a Message, so boxing it would only add an allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum LiveEvent {
    /// Every MESSAGE_CREATE; `own` when the user wrote it (from any device).
    Message {
        msg: Message,
        own: bool,
    },
    Acked {
        channel: Id,
        message: Id,
    },
    Ready,
}

struct Inner {
    /// The gateway's own state. `halt`, when set, overrides it.
    conn: ConnState,
    /// Set by a halting error (gateway or REST). Only `clear_halt` or a new
    /// login lifts it: reconnects, RESUMED and READY never do.
    halt: Option<DiscordError>,
    /// SHA-256 of the token last rejected or last tried after a halt.
    login_token: Option<[u8; 32]>,
    me: Option<UserRef>,
    guilds: HashMap<Id, Guild>,
    channels: HashMap<Id, Channel>,
    read_states: HashMap<Id, ReadState>,
    /// Keyed by guild; `None` holds the DM settings.
    settings: HashMap<Option<Id>, GuildSettings>,
    users: HashMap<Id, UserRef>,
    buffered: HashSet<Id>,
    recent: HashMap<Id, VecDeque<Message>>,
}

pub struct LiveState {
    policy: BufferPolicy,
    inner: Mutex<Inner>,
    events: broadcast::Sender<LiveEvent>,
}

impl LiveState {
    pub fn new(policy: BufferPolicy) -> LiveState {
        let inner = Inner {
            conn: ConnState::Connecting,
            halt: None,
            login_token: None,
            me: None,
            guilds: HashMap::new(),
            channels: HashMap::new(),
            read_states: HashMap::new(),
            settings: HashMap::new(),
            users: HashMap::new(),
            buffered: HashSet::new(),
            recent: HashMap::new(),
        };
        LiveState { policy, inner: Mutex::new(inner), events: broadcast::channel(EVENT_CAPACITY).0 }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Plain maps stay consistent after a panic elsewhere: keep serving.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, ev: LiveEvent) {
        // No subscriber is fine.
        let _ = self.events.send(ev);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.events.subscribe()
    }

    pub fn policy(&self) -> BufferPolicy {
        self.policy
    }

    /// Updates the model with one gateway event and publishes what changed.
    pub fn apply(&self, ev: GatewayEvent, now_ms: i64) {
        match ev {
            GatewayEvent::Ready(snap) => {
                self.lock().load_ready(*snap, now_ms);
                self.publish(LiveEvent::Ready);
            }
            GatewayEvent::Resumed => self.lock().conn = ConnState::Ready { since_ms: now_ms },
            GatewayEvent::MessageCreate(msg) => {
                let own = self.lock().on_message(&msg, now_ms, self.policy);
                self.publish(LiveEvent::Message { msg: *msg, own });
            }
            GatewayEvent::MessageAck { channel, message, mention_count } => {
                self.lock().read_states.insert(
                    channel,
                    ReadState { last_acked: Some(message), mention_count: mention_count.unwrap_or(0) },
                );
                self.publish(LiveEvent::Acked { channel, message });
            }
            GatewayEvent::ChannelUpsert(c) => self.lock().upsert_channel(c),
            GatewayEvent::ChannelDelete(id) => self.lock().remove_channel(id),
            GatewayEvent::GuildUpsert(g, channels) => {
                let mut inner = self.lock();
                let gid = g.id;
                let keep: HashSet<Id> = channels.iter().map(|c| c.id).collect();
                let gone: Vec<Id> = inner
                    .channels
                    .values()
                    .filter(|c| c.guild_id == Some(gid) && !keep.contains(&c.id))
                    .map(|c| c.id)
                    .collect();
                for id in gone {
                    inner.remove_channel(id);
                }
                inner.guilds.insert(gid, *g);
                for c in channels {
                    inner.upsert_channel(c);
                }
            }
            GatewayEvent::GuildUpdate(g) => {
                let mut inner = self.lock();
                let mut g = *g;
                if let Some(old) = inner.guilds.get(&g.id) {
                    g.my_roles = old.my_roles.clone();
                }
                inner.guilds.insert(g.id, g);
            }
            GatewayEvent::MyRolesUpdate { guild, roles } => {
                if let Some(g) = self.lock().guilds.get_mut(&guild) {
                    g.my_roles = roles;
                }
            }
            GatewayEvent::GuildDelete(id) => {
                let mut inner = self.lock();
                inner.guilds.remove(&id);
                inner.settings.remove(&Some(id));
                let gone: Vec<Id> = inner.channels.values().filter(|c| c.guild_id == Some(id)).map(|c| c.id).collect();
                for c in gone {
                    inner.remove_channel(c);
                }
            }
            GatewayEvent::SettingsUpdate(s) => {
                self.lock().settings.insert(s.guild_id, s);
            }
            GatewayEvent::Disconnected => self.lock().conn = ConnState::Reconnecting,
            GatewayEvent::Halted(e) => self.halt(e),
        }
    }

    /// tritone acked a channel itself (the gateway echoes it later as MESSAGE_ACK).
    pub fn note_acked(&self, channel: Id, message: Id) {
        self.lock().read_states.insert(channel, ReadState { last_acked: Some(message), mention_count: 0 });
        self.publish(LiveEvent::Acked { channel, message });
    }

    /// Stops every Discord call (`AppState::ctx` refuses) until `clear_halt`
    /// or a new login. For errors where `DiscordError::halts()` is true.
    pub fn halt(&self, e: DiscordError) {
        self.lock().halt = Some(e);
    }

    /// Lifts a halt. Only on an explicit user action (e.g. "I solved the
    /// CAPTCHA, continue"), never automatically.
    pub fn clear_halt(&self) {
        self.lock().halt = None;
    }

    /// Discord rejected this token (gateway close 4004, REST 401): halt with
    /// `NeedsLogin` and remember it, so it's never tried again.
    pub fn reject_token(&self, token: &str) {
        let mut inner = self.lock();
        inner.login_token = Some(token_hash(token));
        inner.halt = Some(DiscordError::NeedsLogin);
    }

    /// Called with the keyring's token while halted on `NeedsLogin`. A token
    /// that was rejected, or already tried since the last halt, returns false
    /// (the user must run `tritone login`). A new one lifts the halt, marks the
    /// connection as connecting and returns true: wake the gateway.
    pub fn try_login_with(&self, token: &str) -> bool {
        let hash = token_hash(token);
        let mut inner = self.lock();
        if inner.login_token == Some(hash) {
            return false;
        }
        inner.login_token = Some(hash);
        inner.halt = None;
        inner.conn = ConnState::Connecting;
        true
    }

    /// The gateway's state, or `Halted` while a halt is set.
    pub fn connection(&self) -> ConnState {
        let inner = self.lock();
        inner.halt.clone().map_or_else(|| inner.conn.clone(), ConnState::Halted)
    }

    pub fn me(&self) -> Option<UserRef> {
        self.lock().me.clone()
    }

    pub fn guild(&self, id: Id) -> Option<Guild> {
        self.lock().guilds.get(&id).cloned()
    }

    pub fn guilds(&self) -> Vec<Guild> {
        self.lock().guilds.values().cloned().collect()
    }

    pub fn channel(&self, id: Id) -> Option<Channel> {
        self.lock().channels.get(&id).cloned()
    }

    pub fn channels(&self) -> Vec<Channel> {
        self.lock().channels.values().cloned().collect()
    }

    /// Default (nothing acked, no mentions) when Discord sent no read state.
    pub fn read_state(&self, channel: Id) -> ReadState {
        self.lock().read_states.get(&channel).copied().unwrap_or_default()
    }

    /// `None` = the DM settings.
    pub fn settings(&self, guild: Option<Id>) -> Option<GuildSettings> {
        self.lock().settings.get(&guild).cloned()
    }

    pub fn user(&self, id: Id) -> Option<UserRef> {
        let inner = self.lock();
        inner.users.get(&id).cloned().or_else(|| inner.me.clone().filter(|m| m.id == id))
    }

    /// Every known user (READY's users, DM recipients, authors seen) plus me.
    pub fn users(&self) -> Vec<UserRef> {
        let inner = self.lock();
        let mut all: Vec<UserRef> = inner.users.values().cloned().collect();
        if let Some(me) = inner.me.as_ref().filter(|m| !inner.users.contains_key(&m.id)) {
            all.push(me.clone());
        }
        all
    }

    /// The newest message is past the last acked one. A channel with messages
    /// and no read state counts as unread.
    pub fn is_unread(&self, channel: Id) -> bool {
        let inner = self.lock();
        let last = inner.channels.get(&channel).and_then(|c| c.last_message_id);
        let acked = inner.read_states.get(&channel).and_then(|r| r.last_acked);
        last.is_some() && last > acked
    }

    /// Muted in Discord, as the client counts it: the guild, the channel, its
    /// category, or (for a thread) its parent channel and that one's category
    /// has an active mute. A channel override never unmutes a muted guild.
    /// DMs use the DM settings.
    pub fn discord_muted(&self, channel: Id, now_ms: i64) -> bool {
        let inner = self.lock();
        let Some(c) = inner.channels.get(&channel) else { return false };
        let Some(s) = inner.settings.get(&c.guild_id) else { return false };
        if s.muted.active(now_ms) {
            return true;
        }
        // The channel, then up to two ancestors (thread → channel → category).
        let mut next = Some(c.id);
        for _ in 0..3 {
            let Some(id) = next else { break };
            if s.channel_mutes.get(&id).is_some_and(|m| m.active(now_ms)) {
                return true;
            }
            next = inner.channels.get(&id).and_then(|c| c.parent_id);
        }
        false
    }

    /// Whether the user can see the channel, computed from roles and
    /// overwrites in Discord's order. Threads follow their parent; DMs are
    /// always visible; unknown channels are not.
    pub fn can_view(&self, channel: Id) -> bool {
        let inner = self.lock();
        let Some(mut c) = inner.channels.get(&channel) else { return false };
        if c.kind.is_thread() {
            match c.parent_id.and_then(|p| inner.channels.get(&p)) {
                Some(parent) => c = parent,
                None => return false,
            }
        }
        let Some(gid) = c.guild_id else { return true };
        let (Some(g), Some(me)) = (inner.guilds.get(&gid), inner.me.as_ref()) else { return false };
        channel_permissions(g, c, me.id) & VIEW_CHANNEL != 0
    }

    /// Buffered messages, oldest first.
    pub fn recent(&self, channel: Id) -> Vec<Message> {
        self.lock().recent.get(&channel).map(|q| q.iter().cloned().collect()).unwrap_or_default()
    }

    /// Which channels keep recent text (DMs and group DMs always do). Channels
    /// dropped from the set lose what they held.
    pub fn set_buffered(&self, channels: HashSet<Id>) {
        let mut inner = self.lock();
        inner.buffered = channels;
        let Inner { recent, buffered, channels, .. } = &mut *inner;
        recent.retain(|id, _| buffered.contains(id) || channels.get(id).is_some_and(|c| c.kind.is_private()));
    }
}

/// Discord's permission order: @everyone plus the member's roles; owner and
/// administrator get everything; then the @everyone overwrite, the role
/// overwrites combined, and the member overwrite, each deny then allow.
/// https://docs.discord.food/topics/permissions#permission-overwrites
fn channel_permissions(g: &Guild, c: &Channel, me: Id) -> u64 {
    if g.owner_id == me {
        return u64::MAX;
    }
    let role_perms = |id: Id| g.roles.iter().find(|r| r.id == id).map_or(0, |r| r.permissions);
    let mut perms = g.my_roles.iter().fold(role_perms(g.id), |acc, r| acc | role_perms(*r));
    if perms & ADMINISTRATOR != 0 {
        return u64::MAX;
    }
    if let Some(o) = c.overwrites.iter().find(|o| !o.is_member && o.id == g.id) {
        perms = (perms & !o.deny) | o.allow;
    }
    let (allow, deny) = c
        .overwrites
        .iter()
        .filter(|o| !o.is_member && g.my_roles.contains(&o.id))
        .fold((0, 0), |(a, d), o| (a | o.allow, d | o.deny));
    perms = (perms & !deny) | allow;
    if let Some(o) = c.overwrites.iter().find(|o| o.is_member && o.id == me) {
        perms = (perms & !o.deny) | o.allow;
    }
    perms
}

impl Inner {
    fn load_ready(&mut self, snap: ReadySnapshot, now_ms: i64) {
        self.conn = ConnState::Ready { since_ms: now_ms };
        self.guilds = snap.guilds.into_iter().map(|g| (g.id, g)).collect();
        self.channels.clear();
        self.users = snap.users.into_iter().map(|u| (u.id, u)).collect();
        for c in snap.channels {
            self.upsert_channel(c);
        }
        self.read_states = snap.read_states;
        // Discord keeps settings for guilds the user left: ignore those.
        let guilds = &self.guilds;
        self.settings = snap
            .settings
            .into_iter()
            .filter(|s| s.guild_id.is_none_or(|g| guilds.contains_key(&g)))
            .map(|s| (s.guild_id, s))
            .collect();
        let channels = &self.channels;
        self.recent.retain(|id, _| channels.contains_key(id));
        self.me = Some(snap.me);
    }

    /// Keeps the newest known `last_message_id`: an update may be older than
    /// a MESSAGE_CREATE already seen.
    fn upsert_channel(&mut self, mut c: Channel) {
        let known = self.channels.get(&c.id).and_then(|old| old.last_message_id);
        c.last_message_id = c.last_message_id.max(known);
        for u in &c.recipients {
            self.users.insert(u.id, u.clone());
        }
        self.channels.insert(c.id, c);
    }

    fn remove_channel(&mut self, id: Id) {
        self.channels.remove(&id);
        self.read_states.remove(&id);
        self.recent.remove(&id);
    }

    /// Returns whether the user wrote it.
    fn on_message(&mut self, msg: &Message, now_ms: i64, policy: BufferPolicy) -> bool {
        let me = self.me.as_ref().map(|u| u.id);
        let own = me == Some(msg.author.id);
        let pinged = self.pings_me(msg, me);
        let ch = self.channels.entry(msg.channel_id).or_insert_with(|| new_channel_for(msg));
        ch.last_message_id = ch.last_message_id.max(Some(msg.id));
        let private = ch.kind.is_private();
        let rs = self.read_states.entry(msg.channel_id).or_default();
        if own {
            // Discord acks your own messages.
            *rs = ReadState { last_acked: Some(msg.id), mention_count: 0 };
        } else if private || pinged {
            rs.mention_count += 1;
        }
        self.users.entry(msg.author.id).or_insert_with(|| msg.author.clone());
        if private || self.buffered.contains(&msg.channel_id) {
            let q = self.recent.entry(msg.channel_id).or_default();
            q.push_back(msg.clone());
            let cutoff = now_ms - policy.max_age_ms;
            while q.front().is_some_and(|m| m.timestamp_ms < cutoff) || q.len() > policy.per_channel {
                q.pop_front();
            }
        }
        own
    }
}

impl Inner {
    /// Mentions the user directly, through one of their roles, or with
    /// @everyone/@here, unless the server suppresses role or @everyone pings.
    fn pings_me(&self, msg: &Message, me: Option<Id>) -> bool {
        if me.is_some_and(|m| msg.mentions.contains(&m)) {
            return true;
        }
        let gid = self.channels.get(&msg.channel_id).and_then(|c| c.guild_id).or(msg.guild_id);
        let Some(gid) = gid else { return false };
        let s = self.settings.get(&Some(gid));
        let roles = !s.is_some_and(|s| s.suppress_roles)
            && self.guilds.get(&gid).is_some_and(|g| msg.mention_roles.iter().any(|r| g.my_roles.contains(r)));
        let everyone = msg.mention_everyone && !s.is_some_and(|s| s.suppress_everyone);
        roles || everyone
    }
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// A channel first seen through a message (e.g. a new DM before CHANNEL_CREATE).
fn new_channel_for(msg: &Message) -> Channel {
    let dm = msg.guild_id.is_none();
    Channel {
        id: msg.channel_id,
        guild_id: msg.guild_id,
        parent_id: None,
        name: None,
        kind: if dm { ChannelKind::Dm } else { ChannelKind::Text },
        recipients: if dm { vec![msg.author.clone()] } else { vec![] },
        last_message_id: None,
        overwrites: vec![],
        position: 0,
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
