//! The contribution log: one row per message the user posted in a server.
//! Ids, times and a kind only, never text.

use anyhow::Result;
use rusqlite::{Row, params};

use super::Store;
use crate::discord::types::Id;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    pub message_id: Id,
    pub guild_id: Id,
    pub channel_id: Id,
    /// Unix ms.
    pub created_at: i64,
    /// A topic's kind, `answer`, `other`, or an opportunity rule.
    pub kind: String,
    pub is_reply: bool,
    /// Sent through tritone's queue (not typed in a Discord app).
    pub via_tritone: bool,
}

fn parse_id(s: String) -> rusqlite::Result<Id> {
    s.parse().map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn from_row(r: &Row<'_>) -> rusqlite::Result<ActivityRow> {
    Ok(ActivityRow {
        message_id: parse_id(r.get(0)?)?,
        guild_id: parse_id(r.get(1)?)?,
        channel_id: parse_id(r.get(2)?)?,
        created_at: r.get(3)?,
        kind: r.get(4)?,
        is_reply: r.get(5)?,
        via_tritone: r.get(6)?,
    })
}

impl Store {
    /// Insert-or-ignore: the gateway echo and the queue may both record the
    /// same message, and the first one wins. Returns whether it was new.
    pub fn record_activity(&self, row: &ActivityRow) -> Result<bool> {
        let n = self.conn().execute(
            "INSERT OR IGNORE INTO activity
                (message_id, guild_id, channel_id, created_at, kind, is_reply, via_tritone)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.message_id.to_string(),
                row.guild_id.to_string(),
                row.channel_id.to_string(),
                row.created_at,
                row.kind,
                row.is_reply,
                row.via_tritone
            ],
        )?;
        Ok(n == 1)
    }

    /// Flags a logged message as sent through tritone. Returns false when
    /// there's no row for it yet.
    pub fn mark_sent_via_tritone(&self, message_id: Id) -> Result<bool> {
        let n = self
            .conn()
            .execute("UPDATE activity SET via_tritone = 1 WHERE message_id = ?1", [message_id.to_string()])?;
        Ok(n == 1)
    }

    /// An explicit kind (an opportunity rule, or one Claude chose) beats the
    /// inferred one.
    pub fn set_activity_kind(&self, message_id: Id, kind: &str) -> Result<()> {
        self.conn()
            .execute("UPDATE activity SET kind = ?2 WHERE message_id = ?1", params![message_id.to_string(), kind])?;
        Ok(())
    }

    /// Rows with `from_ms <= created_at < to_ms`, newest first.
    pub fn activity_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<ActivityRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT message_id, guild_id, channel_id, created_at, kind, is_reply, via_tritone
             FROM activity WHERE created_at >= ?1 AND created_at < ?2
             ORDER BY created_at DESC, CAST(message_id AS INTEGER) DESC",
        )?;
        let rows = stmt.query_map(params![from_ms, to_ms], from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
