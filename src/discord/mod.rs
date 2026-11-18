//! Everything that talks to Discord. Nothing outside this module performs
//! network calls.

pub mod api;
#[cfg(test)]
pub mod fake;
pub mod fingerprint;
pub mod gateway;
// Used by the REST client and the gateway, which land after it.
#[allow(dead_code)]
pub(crate) mod parse;
pub mod transport;
pub mod types;

pub use api::{DResult, DiscordApi};
