//! Opportunities: messages in full-watch channels where the user could
//! contribute, kept as ids, the matching rule and a status (never text), so
//! "dismissed" and "answered" stick across restarts.

use anyhow::{Result, bail};
use rusqlite::{OptionalExtension, Row, params};

use super::Store;
use crate::discord::types::Id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpportunityStatus {
    Open,
    Dismissed,
    /// Someone replied (the user, or another person).
    Answered,
    /// The message is gone or out of reach.
    Gone,
    /// A newer message in the same thread took its place (one open
    /// `my_thread` row per thread).
    Superseded,
}

impl OpportunityStatus {
    fn as_str(self) -> &'static str {
        match self {
            OpportunityStatus::Open => "open",
            OpportunityStatus::Dismissed => "dismissed",
            OpportunityStatus::Answered => "answered",
            OpportunityStatus::Gone => "gone",
            OpportunityStatus::Superseded => "superseded",
        }
    }

    fn parse(s: &str) -> Result<OpportunityStatus> {
        Ok(match s {
            "open" => OpportunityStatus::Open,
            "dismissed" => OpportunityStatus::Dismissed,
            "answered" => OpportunityStatus::Answered,
            "gone" => OpportunityStatus::Gone,
            "superseded" => OpportunityStatus::Superseded,
            other => bail!("unknown opportunity status in db: {other}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpportunityRow {
    pub message_id: Id,
    pub channel_id: Id,
    pub guild_id: Id,
    /// `question`, `topic:<kind>` or `my_thread`.
    pub rule: String,
    /// The message's time, Unix ms.
    pub created_at: i64,
    pub status: OpportunityStatus,
    pub updated_at: i64,
}

fn parse_id(s: String) -> rusqlite::Result<Id> {
    s.parse().map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<OpportunityRow> {
    let status: String = r.get(5)?;
    Ok(OpportunityRow {
        message_id: parse_id(r.get(0)?)?,
        channel_id: parse_id(r.get(1)?)?,
        guild_id: parse_id(r.get(2)?)?,
        rule: r.get(3)?,
        created_at: r.get(4)?,
        status: OpportunityStatus::parse(&status)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into()))?,
        updated_at: r.get(6)?,
    })
}

const COLUMNS: &str = "message_id, channel_id, guild_id, rule, created_at, status, updated_at";

impl Store {
    /// Insert-or-ignore: the first rule that matched a message wins, and a
    /// dismissed one never reopens. Returns whether it was new.
    pub fn add_opportunity(&self, row: &OpportunityRow) -> Result<bool> {
        let n = self.conn().execute(
            &format!("INSERT OR IGNORE INTO opportunity ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
            params![
                row.message_id.to_string(),
                row.channel_id.to_string(),
                row.guild_id.to_string(),
                row.rule,
                row.created_at,
                row.status.as_str(),
                row.updated_at
            ],
        )?;
        Ok(n == 1)
    }

    pub fn opportunity(&self, message_id: Id) -> Result<Option<OpportunityRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM opportunity WHERE message_id = ?1"),
                [message_id.to_string()],
                from_row,
            )
            .optional()?)
    }

    /// Open rows for messages from `since_ms` on, newest first.
    pub fn open_opportunities(&self, since_ms: i64) -> Result<Vec<OpportunityRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM opportunity WHERE status = 'open' AND created_at >= ?1
             ORDER BY created_at DESC, CAST(message_id AS INTEGER) DESC"
        ))?;
        let rows = stmt.query_map([since_ms], from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Returns false when there's no row for the message.
    pub fn set_opportunity_status(&self, message_id: Id, status: OpportunityStatus, now_ms: i64) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE opportunity SET status = ?2, updated_at = ?3 WHERE message_id = ?1",
            params![message_id.to_string(), status.as_str(), now_ms],
        )?;
        Ok(n == 1)
    }

    /// The user replied to this message (through tritone or a Discord app):
    /// an open opportunity for it is answered. Returns whether one was.
    pub fn opportunity_answered(&self, message_id: Id, now_ms: i64) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE opportunity SET status = 'answered', updated_at = ?2 WHERE message_id = ?1 AND status = 'open'",
            params![message_id.to_string(), now_ms],
        )?;
        Ok(n == 1)
    }

    /// The user wrote `own` in `channel` (a thread): every open row there for
    /// an older message is answered, reply-quote or not. Returns how many.
    pub fn opportunities_answered_before(&self, channel: Id, own: Id, now_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE opportunity SET status = 'answered', updated_at = ?3
             WHERE channel_id = ?1 AND status = 'open' AND CAST(message_id AS INTEGER) < CAST(?2 AS INTEGER)",
            params![channel.to_string(), own.to_string(), now_ms],
        )?)
    }

    /// Keeps only the newest open `my_thread` row in `channel`; older ones
    /// are superseded. Returns how many.
    pub fn keep_newest_my_thread(&self, channel: Id, now_ms: i64) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE opportunity SET status = 'superseded', updated_at = ?2
             WHERE channel_id = ?1 AND rule = 'my_thread' AND status = 'open'
               AND CAST(message_id AS INTEGER) < (
                   SELECT MAX(CAST(message_id AS INTEGER)) FROM opportunity
                   WHERE channel_id = ?1 AND rule = 'my_thread' AND status = 'open')",
            params![channel.to_string(), now_ms],
        )?)
    }

    /// The user wrote in this channel (a thread), per `own_message` or the contribution log.
    pub fn wrote_in(&self, channel: Id) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM own_message WHERE channel_id = ?1)
                 OR EXISTS (SELECT 1 FROM activity WHERE channel_id = ?1)",
            [channel.to_string()],
            |r| r.get(0),
        )?)
    }

    /// The message is one of the user's, per `own_message` or the contribution log.
    pub fn is_own_message(&self, message: Id) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM own_message WHERE message_id = ?1)
                 OR EXISTS (SELECT 1 FROM activity WHERE message_id = ?1)",
            [message.to_string()],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
#[path = "opportunity_tests.rs"]
mod tests;
