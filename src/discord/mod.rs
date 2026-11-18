//! Everything that talks to Discord. Nothing outside this module performs
//! network calls.

pub mod api;
#[cfg(test)]
pub mod fake;
pub mod fingerprint;
pub mod gateway;
pub mod http;
pub(crate) mod parse;
pub mod rate;
pub mod transport;
pub mod types;

pub use api::{DResult, DiscordApi};
