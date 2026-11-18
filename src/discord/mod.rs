//! Everything that talks to Discord. Nothing outside this module performs
//! network calls.

pub mod api;
pub mod gateway;
pub mod types;

pub use api::{DResult, DiscordApi};
