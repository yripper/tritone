//! The REST surface the rest of tritone uses. `HttpDiscord` implements it;
//! tests use `FakeDiscord`.

use async_trait::async_trait;

use super::types::{
    Channel, DiscordError, Id, Message, MuteTarget, OutgoingMessage, SearchPage, SearchQuery, SearchScope, UserRef,
};

pub type DResult<T> = Result<T, DiscordError>;

#[async_trait]
pub trait DiscordApi: Send + Sync {
    /// Newest first. At most one of before/after.
    async fn history(&self, channel: Id, before: Option<Id>, after: Option<Id>, limit: u8) -> DResult<Vec<Message>>;
    /// Messages around one message (user accounts can't fetch a single message).
    async fn around(&self, channel: Id, message: Id, limit: u8) -> DResult<Vec<Message>>;
    async fn recent_mentions(&self, limit: u8, before: Option<Id>) -> DResult<Vec<Message>>;
    async fn search(&self, scope: SearchScope, query: &SearchQuery) -> DResult<SearchPage>;
    async fn ack(&self, channel: Id, message: Id) -> DResult<()>;
    /// At most 100 items; callers chunk.
    async fn ack_bulk(&self, items: &[(Id, Id)]) -> DResult<()>;
    async fn typing(&self, channel: Id) -> DResult<()>;
    async fn send(&self, channel: Id, msg: &OutgoingMessage) -> DResult<Message>;
    async fn react(&self, channel: Id, message: Id, emoji: &str) -> DResult<()>;
    async fn create_thread(&self, channel: Id, from_message: Option<Id>, name: &str) -> DResult<Channel>;
    async fn create_forum_post(&self, forum: Id, name: &str, content: &str, nonce: Id) -> DResult<(Channel, Message)>;
    async fn leave_guild(&self, guild: Id) -> DResult<()>;
    async fn set_mute(&self, target: MuteTarget, muted: bool) -> DResult<()>;
    async fn user(&self, user: Id) -> DResult<UserRef>;

    /// Lifts a halt the client keeps itself (e.g. a sticky CAPTCHA), so calls
    /// reach Discord again. Called only by `SendQueue::resume`, on the user's
    /// explicit OK, together with `LiveState::clear_halt`. No-op by default.
    fn clear_halt(&self) {}
}
