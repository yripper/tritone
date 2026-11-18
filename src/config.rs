//! Runtime paths and user config. All state lives in `~/.tritone` (or
//! `TRITONE_HOME`), never in the repo.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_PORT: u16 = 7781;

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub db: PathBuf,
    pub config: PathBuf,
    pub style: PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Paths> {
        let home = match std::env::var_os("TRITONE_HOME") {
            Some(h) if !h.is_empty() => PathBuf::from(h),
            _ => {
                let user_home = std::env::var_os("HOME").context("HOME is not set")?;
                PathBuf::from(user_home).join(".tritone")
            }
        };
        std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
        // Holds the DB with triage state and names: owner-only.
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
        Ok(Paths { db: home.join("tritone.db"), config: home.join("config.json"), style: home.join("style.md"), home })
    }
}

/// A subject the user wants to hear about, e.g. `{"kind": "rust", "keywords": ["rust", "cargo"]}`.
/// `kind` labels the opportunities (and contribution log entries) it produces.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Topic {
    pub kind: String,
    pub keywords: Vec<String>,
}

/// `~/.tritone/config.json`. Every field is optional; a missing file means defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    /// Empty by default: the user fills them in.
    pub topics: Vec<Topic>,
    /// A question counts as unanswered after this long without a reply...
    pub unanswered_after_mins: u32,
    /// ...and stops being a candidate once older than this.
    pub unanswered_max_age_hours: u32,
    /// Recent-message buffer per channel (full-watch channels and DMs only).
    pub recent_per_channel: usize,
    pub recent_max_age_hours: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: DEFAULT_PORT,
            topics: Vec::new(),
            unanswered_after_mins: 30,
            unanswered_max_age_hours: 48,
            recent_per_channel: 200,
            recent_max_age_hours: 24,
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Config> {
        match std::fs::read_to_string(&paths.config) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", paths.config.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
