//! Voice: tone profiles per category, notes per channel (both style
//! descriptions Claude writes), categories per channel or server, and
//! `own_message`, the ids and times of the user's own messages tritone has
//! seen, used to count how much they wrote since a profile was saved.
//! Personal data: never log bodies.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{OptionalExtension, params, params_from_iter};

use super::Store;
use crate::discord::types::Id;

/// Older `own_message` rows can't affect staleness (anything saved before the
/// 21-day window is stale by age anyway), so they are pruned.
pub const OWN_MESSAGE_KEEP_MS: i64 = 22 * 86_400_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceEntry {
    pub body: String,
    /// Unix ms.
    pub updated_at: i64,
}

/// One of the user's own messages, as ids and a time (no text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnMessageRow {
    pub message_id: Id,
    pub channel_id: Id,
    pub guild_id: Option<Id>,
    /// Unix ms.
    pub created_at: i64,
}

/// Messages sent through tritone are Claude's wording, not the user's voice.
const NOT_VIA_TRITONE: &str =
    "NOT EXISTS (SELECT 1 FROM activity a WHERE a.message_id = o.message_id AND a.via_tritone = 1)";

fn parse_id(s: String) -> rusqlite::Result<Id> {
    s.parse().map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn parse_opt_id(s: Option<String>) -> rusqlite::Result<Option<Id>> {
    s.map(parse_id).transpose()
}

impl Store {
    pub fn profile(&self, category: &str) -> Result<Option<VoiceEntry>> {
        self.voice_entry("SELECT body, updated_at FROM voice_profile WHERE category = ?1", category)
    }

    pub fn save_profile(&self, category: &str, body: &str, now_ms: i64) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO voice_profile (category, body, updated_at) VALUES (?1, ?2, ?3)",
            params![category, body, now_ms],
        )?;
        Ok(())
    }

    /// (category, updated_at) only, so listing never exposes profile text.
    pub fn profile_dates(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT category, updated_at FROM voice_profile ORDER BY category")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn notes(&self, channel: Id) -> Result<Option<VoiceEntry>> {
        self.voice_entry("SELECT body, updated_at FROM chat_notes WHERE channel_id = ?1", &channel.to_string())
    }

    pub fn save_notes(&self, channel: Id, body: &str, now_ms: i64) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO chat_notes (channel_id, body, updated_at) VALUES (?1, ?2, ?3)",
            params![channel.to_string(), body, now_ms],
        )?;
        Ok(())
    }

    fn voice_entry(&self, sql: &str, key: &str) -> Result<Option<VoiceEntry>> {
        Ok(self
            .conn()
            .query_row(sql, [key], |r| Ok(VoiceEntry { body: r.get(0)?, updated_at: r.get(1)? }))
            .optional()?)
    }

    /// The category set on this channel or server, if any.
    pub fn category(&self, target: Id) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT category FROM category WHERE target_id = ?1", [target.to_string()], |r| r.get(0))
            .optional()?)
    }

    pub fn set_category(&self, target: Id, category: &str) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO category (target_id, category) VALUES (?1, ?2)",
            params![target.to_string(), category],
        )?;
        Ok(())
    }

    /// Channels and servers set to this category.
    pub fn targets_in_category(&self, category: &str) -> Result<Vec<Id>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT target_id FROM category WHERE category = ?1 ORDER BY target_id")?;
        let rows = stmt.query_map([category], |r| parse_id(r.get(0)?))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records own messages (ids and times only). Seeing the same message
    /// twice counts once. Prunes rows past the keep window.
    pub fn record_own_messages(&self, rows: &[OwnMessageRow], now_ms: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for r in rows {
            tx.execute(
                "INSERT OR IGNORE INTO own_message (message_id, channel_id, guild_id, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    r.message_id.to_string(),
                    r.channel_id.to_string(),
                    r.guild_id.map(|g| g.to_string()),
                    r.created_at
                ],
            )?;
        }
        tx.execute("DELETE FROM own_message WHERE created_at < ?1", [now_ms - OWN_MESSAGE_KEEP_MS])?;
        tx.commit()?;
        Ok(())
    }

    /// Own messages (not sent through tritone) in a channel strictly after `since_ms`.
    pub fn own_count_in_channel(&self, channel: Id, since_ms: i64) -> Result<u32> {
        let sql = format!(
            "SELECT COUNT(*) FROM own_message o WHERE o.channel_id = ?1 AND o.created_at > ?2 AND {NOT_VIA_TRITONE}"
        );
        Ok(self.conn().query_row(&sql, params![channel.to_string(), since_ms], |r| r.get(0))?)
    }

    /// (channel, guild) of every own message (not sent through tritone)
    /// strictly after `since_ms`. Callers group them by category, which
    /// depends on the live model (a thread follows its parent channel).
    pub fn own_since(&self, since_ms: i64) -> Result<Vec<(Id, Option<Id>)>> {
        let sql =
            format!("SELECT o.channel_id, o.guild_id FROM own_message o WHERE o.created_at > ?1 AND {NOT_VIA_TRITONE}");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([since_ms], |r| Ok((parse_id(r.get(0)?)?, parse_opt_id(r.get(1)?)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Channels the user wrote in, newest first, from both `own_message` and
    /// the contribution log (which keeps older server messages).
    pub fn recent_own_channels(&self, limit: usize) -> Result<Vec<(Id, Option<Id>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT channel_id, MAX(guild_id), MAX(created_at) AS last FROM (
                SELECT channel_id, guild_id, created_at FROM own_message
                UNION ALL SELECT channel_id, guild_id, created_at FROM activity
             ) GROUP BY channel_id ORDER BY last DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| Ok((parse_id(r.get(0)?)?, parse_opt_id(r.get(1)?)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The newest own message in a server that tritone knows of (Unix ms),
    /// from `own_message` and the contribution log.
    pub fn last_own_post_in_guild(&self, guild: Id) -> Result<Option<i64>> {
        Ok(self.conn().query_row(
            "SELECT MAX(created_at) FROM (
                SELECT created_at FROM own_message WHERE guild_id = ?1
                UNION ALL SELECT created_at FROM activity WHERE guild_id = ?1
             )",
            [guild.to_string()],
            |r| r.get(0),
        )?)
    }

    /// Channels of a server where tritone knows the user wrote.
    pub fn own_channels_in_guild(&self, guild: Id) -> Result<HashSet<Id>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT channel_id FROM own_message WHERE guild_id = ?1
             UNION SELECT channel_id FROM activity WHERE guild_id = ?1",
        )?;
        let rows = stmt.query_map([guild.to_string()], |r| parse_id(r.get(0)?))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Which of these messages went out through tritone's queue.
    pub fn sent_via_tritone(&self, ids: &[Id]) -> Result<HashSet<Id>> {
        if ids.is_empty() {
            return Ok(HashSet::new());
        }
        let marks = vec!["?"; ids.len()].join(",");
        let sql = format!("SELECT message_id FROM activity WHERE via_tritone = 1 AND message_id IN ({marks})");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(ids.iter().map(|i| i.to_string())), |r| parse_id(r.get(0)?))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
