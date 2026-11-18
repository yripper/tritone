use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::Store;
use crate::discord::types::Id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WatchLevel {
    /// Any unread message makes the channel pending, and it is scanned for opportunities.
    Full,
    /// Pending only on a mention or a reply (the default).
    Normal,
    /// Never pending, hidden from listings.
    Muted,
}

impl WatchLevel {
    fn as_str(self) -> &'static str {
        match self {
            WatchLevel::Full => "full",
            WatchLevel::Normal => "normal",
            WatchLevel::Muted => "muted",
        }
    }

    fn parse(s: &str) -> Result<WatchLevel> {
        Ok(match s {
            "full" => WatchLevel::Full,
            "normal" => WatchLevel::Normal,
            "muted" => WatchLevel::Muted,
            other => bail!("unknown watch level in db: {other}"),
        })
    }
}

/// What a watch level is set on. A channel's level overrides its server's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Guild,
    Channel,
}

impl TargetKind {
    fn as_str(self) -> &'static str {
        match self {
            TargetKind::Guild => "guild",
            TargetKind::Channel => "channel",
        }
    }
}

fn parse_id(s: &str) -> Result<Id> {
    Ok(Id(s.parse().with_context(|| format!("bad id in db: {s}"))?))
}

impl Store {
    pub fn get_watch(&self, target: Id) -> Result<Option<WatchLevel>> {
        let level: Option<String> = self
            .conn()
            .query_row("SELECT level FROM watch WHERE target_id = ?1", [target.0.to_string()], |r| r.get(0))
            .optional()?;
        level.as_deref().map(WatchLevel::parse).transpose()
    }

    /// `None` clears the level, so the default for the target applies again.
    pub fn set_watch(&self, target: Id, kind: TargetKind, level: Option<WatchLevel>) -> Result<()> {
        let id = target.0.to_string();
        match level {
            Some(l) => self.conn().execute(
                "INSERT INTO watch (target_id, kind, level) VALUES (?1, ?2, ?3)
                 ON CONFLICT(target_id) DO UPDATE SET kind = excluded.kind, level = excluded.level",
                params![id, kind.as_str(), l.as_str()],
            )?,
            None => self.conn().execute("DELETE FROM watch WHERE target_id = ?1", [id])?,
        };
        Ok(())
    }

    /// Every explicit level, guilds and channels together (ids never collide).
    pub fn all_watch(&self) -> Result<HashMap<Id, WatchLevel>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT target_id, level FROM watch")?;
        let rows: Vec<(String, String)> =
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        rows.iter().map(|(id, level)| Ok((parse_id(id)?, WatchLevel::parse(level)?))).collect()
    }

    /// Unix ms. `None` unsnoozes.
    pub fn set_snooze(&self, channel: Id, until: Option<i64>) -> Result<()> {
        let id = channel.0.to_string();
        match until {
            Some(t) => self.conn().execute(
                "INSERT INTO snooze (channel_id, until) VALUES (?1, ?2)
                 ON CONFLICT(channel_id) DO UPDATE SET until = excluded.until",
                params![id, t],
            )?,
            None => self.conn().execute("DELETE FROM snooze WHERE channel_id = ?1", [id])?,
        };
        Ok(())
    }

    /// The stored time, even if already past: callers compare it with now.
    pub fn snoozed_until(&self, channel: Id) -> Result<Option<i64>> {
        Ok(self
            .conn()
            .query_row("SELECT until FROM snooze WHERE channel_id = ?1", [channel.0.to_string()], |r| r.get(0))
            .optional()?)
    }
}
