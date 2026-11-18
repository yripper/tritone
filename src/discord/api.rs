//! The REST surface the rest of tritone uses. `HttpDiscord` implements it;
//! tests use `FakeDiscord`.

use async_trait::async_trait;

use super::types::DiscordError;

pub type DResult<T> = Result<T, DiscordError>;

#[async_trait]
pub trait DiscordApi: Send + Sync {}
