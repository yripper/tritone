//! `FakeDiscord`: an in-memory `DiscordApi` for tests. It records every call
//! and can be programmed per method.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::api::{DResult, DiscordApi};
use super::types::{
    Channel, ChannelKind, DiscordError, Id, Message, MuteTarget, OutgoingMessage, SearchPage, SearchQuery, SearchScope,
    UserRef,
};

/// Every call `FakeDiscord` received, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    History { channel: Id, before: Option<Id>, after: Option<Id>, limit: u8 },
    Around { channel: Id, message: Id, limit: u8 },
    RecentMentions { limit: u8, before: Option<Id> },
    Search { scope: SearchScope, query: SearchQuery },
    Ack { channel: Id, message: Id },
    AckBulk(Vec<(Id, Id)>),
    Typing(Id),
    Send { channel: Id, msg: OutgoingMessage },
    React { channel: Id, message: Id, emoji: String },
    CreateThread { channel: Id, from_message: Option<Id>, name: String },
    CreateForumPost { forum: Id, name: String, content: String, nonce: Id },
    LeaveGuild(Id),
    SetMute { target: MuteTarget, muted: bool },
    User(Id),
}

/// Search pages hold this many messages, like Discord.
pub const SEARCH_PAGE: usize = 25;

pub struct FakeDiscord {
    /// Who `send` posts as.
    pub me: UserRef,
    pub calls: Mutex<Vec<Call>>,
    /// Messages per channel, any order. `send` and `create_forum_post` add to them.
    pub history_pages: Mutex<HashMap<Id, Vec<Message>>>,
    /// What `recent_mentions` returns (newest first after sorting).
    pub mentions: Mutex<Vec<Message>>,
    /// What `search` filters and pages through.
    pub search_index: Mutex<Vec<Message>>,
    pub users: Mutex<HashMap<Id, UserRef>>,
    /// Returned once, by the next call (which is still recorded).
    pub next_error: Mutex<Option<DiscordError>>,
    pub sent: Mutex<Vec<(Id, OutgoingMessage)>>,
}

impl Default for FakeDiscord {
    fn default() -> Self {
        FakeDiscord::new(UserRef { id: Id(1), username: "U001".into(), display_name: None, bot: false })
    }
}

fn newest_first(mut v: Vec<Message>) -> Vec<Message> {
    v.sort_by_key(|m| std::cmp::Reverse(m.id));
    v
}

impl FakeDiscord {
    pub fn new(me: UserRef) -> FakeDiscord {
        FakeDiscord {
            me,
            calls: Mutex::new(Vec::new()),
            history_pages: Mutex::new(HashMap::new()),
            mentions: Mutex::new(Vec::new()),
            search_index: Mutex::new(Vec::new()),
            users: Mutex::new(HashMap::new()),
            next_error: Mutex::new(None),
            sent: Mutex::new(Vec::new()),
        }
    }

    /// Adds messages to a channel's history.
    pub fn put_history(&self, channel: Id, msgs: Vec<Message>) {
        self.history_pages.lock().unwrap().entry(channel).or_default().extend(msgs);
    }

    pub fn fail_next(&self, e: DiscordError) {
        *self.next_error.lock().unwrap() = Some(e);
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// Records the call, then returns the programmed error if there is one.
    fn record(&self, call: Call) -> DResult<()> {
        self.calls.lock().unwrap().push(call);
        match self.next_error.lock().unwrap().take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn channel_messages(&self, channel: Id) -> Vec<Message> {
        newest_first(self.history_pages.lock().unwrap().get(&channel).cloned().unwrap_or_default())
    }

    fn message_from(&self, channel: Id, msg: &OutgoingMessage) -> Message {
        Message {
            id: msg.nonce,
            channel_id: channel,
            guild_id: None,
            author: self.me.clone(),
            content: msg.content.clone(),
            timestamp_ms: msg.nonce.timestamp_ms(),
            edited: false,
            reply_to: msg.reply_to,
            reply_to_author: None,
            mentions: vec![],
            mention_roles: vec![],
            mention_everyone: false,
            attachments: vec![],
            embeds: vec![],
            reactions: vec![],
            kind: if msg.reply_to.is_some() { 19 } else { 0 },
        }
    }
}

#[async_trait]
impl DiscordApi for FakeDiscord {
    async fn history(&self, channel: Id, before: Option<Id>, after: Option<Id>, limit: u8) -> DResult<Vec<Message>> {
        self.record(Call::History { channel, before, after, limit })?;
        let all = self.channel_messages(channel);
        let picked: Vec<Message> = match after {
            // Discord returns the `limit` messages right after `after`, still newest first.
            Some(a) => {
                let mut v: Vec<Message> = all.into_iter().filter(|m| m.id > a).collect();
                let skip = v.len().saturating_sub(usize::from(limit));
                v.drain(..skip);
                v
            }
            None => all.into_iter().filter(|m| before.is_none_or(|b| m.id < b)).take(usize::from(limit)).collect(),
        };
        Ok(picked)
    }

    async fn around(&self, channel: Id, message: Id, limit: u8) -> DResult<Vec<Message>> {
        self.record(Call::Around { channel, message, limit })?;
        let all = self.channel_messages(channel);
        let Some(at) = all.iter().position(|m| m.id <= message) else { return Ok(vec![]) };
        let start = at.saturating_sub(usize::from(limit) / 2);
        Ok(all.into_iter().skip(start).take(usize::from(limit)).collect())
    }

    async fn recent_mentions(&self, limit: u8, before: Option<Id>) -> DResult<Vec<Message>> {
        self.record(Call::RecentMentions { limit, before })?;
        let all = newest_first(self.mentions.lock().unwrap().clone());
        Ok(all.into_iter().filter(|m| before.is_none_or(|b| m.id < b)).take(usize::from(limit)).collect())
    }

    async fn search(&self, scope: SearchScope, query: &SearchQuery) -> DResult<SearchPage> {
        self.record(Call::Search { scope, query: query.clone() })?;
        let hits: Vec<Message> = newest_first(self.search_index.lock().unwrap().clone())
            .into_iter()
            .filter(|m| match scope {
                SearchScope::Guild(g) => m.guild_id == Some(g),
                SearchScope::Channel(c) => m.channel_id == c,
            })
            .filter(|m| query.author_id.is_none_or(|a| m.author.id == a))
            .filter(|m| query.channel_id.is_none_or(|c| m.channel_id == c))
            .filter(|m| query.min_id.is_none_or(|x| m.id > x))
            .filter(|m| query.max_id.is_none_or(|x| m.id < x))
            .filter(|m| query.content.as_deref().is_none_or(|t| m.content.contains(t)))
            .collect();
        let total = hits.len() as u32;
        let messages = hits.into_iter().skip(query.offset as usize).take(SEARCH_PAGE).collect();
        Ok(SearchPage { total, messages })
    }

    async fn ack(&self, channel: Id, message: Id) -> DResult<()> {
        self.record(Call::Ack { channel, message })
    }

    async fn ack_bulk(&self, items: &[(Id, Id)]) -> DResult<()> {
        self.record(Call::AckBulk(items.to_vec()))
    }

    async fn typing(&self, channel: Id) -> DResult<()> {
        self.record(Call::Typing(channel))
    }

    async fn send(&self, channel: Id, msg: &OutgoingMessage) -> DResult<Message> {
        self.record(Call::Send { channel, msg: msg.clone() })?;
        self.sent.lock().unwrap().push((channel, msg.clone()));
        let m = self.message_from(channel, msg);
        self.put_history(channel, vec![m.clone()]);
        Ok(m)
    }

    async fn react(&self, channel: Id, message: Id, emoji: &str) -> DResult<()> {
        self.record(Call::React { channel, message, emoji: emoji.to_string() })
    }

    /// A thread started from a message takes that message's id, as on Discord.
    async fn create_thread(&self, channel: Id, from_message: Option<Id>, name: &str) -> DResult<Channel> {
        self.record(Call::CreateThread { channel, from_message, name: name.to_string() })?;
        let n = self.calls.lock().unwrap().len() as u64;
        Ok(Channel {
            id: from_message.unwrap_or(Id(1_000_000 + n)),
            guild_id: None,
            parent_id: Some(channel),
            name: Some(name.to_string()),
            kind: ChannelKind::PublicThread,
            recipients: vec![],
            last_message_id: None,
            overwrites: vec![],
            position: 0,
            // The creator is added to the thread.
            joined: true,
        })
    }

    /// The post's thread and starter message both take the nonce as id.
    async fn create_forum_post(&self, forum: Id, name: &str, content: &str, nonce: Id) -> DResult<(Channel, Message)> {
        self.record(Call::CreateForumPost { forum, name: name.to_string(), content: content.to_string(), nonce })?;
        let out = OutgoingMessage { content: content.to_string(), reply_to: None, nonce, silent: false };
        let msg = self.message_from(nonce, &out);
        self.put_history(nonce, vec![msg.clone()]);
        let thread = Channel {
            id: nonce,
            guild_id: None,
            parent_id: Some(forum),
            name: Some(name.to_string()),
            kind: ChannelKind::PublicThread,
            recipients: vec![],
            last_message_id: Some(nonce),
            overwrites: vec![],
            position: 0,
            joined: true,
        };
        Ok((thread, msg))
    }

    async fn leave_guild(&self, guild: Id) -> DResult<()> {
        self.record(Call::LeaveGuild(guild))
    }

    async fn set_mute(&self, target: MuteTarget, muted: bool) -> DResult<()> {
        self.record(Call::SetMute { target, muted })
    }

    async fn user(&self, user: Id) -> DResult<UserRef> {
        self.record(Call::User(user))?;
        self.users.lock().unwrap().get(&user).cloned().ok_or(DiscordError::NotFound)
    }
}

#[cfg(test)]
#[path = "fake_tests.rs"]
mod tests;
