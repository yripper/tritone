use anyhow::Result;
use rusqlite::Connection;

/// Each entry upgrades `user_version` N to N+1. Append only; never edit a shipped one.
/// v1 creates every table up front. Ids are stored as decimal strings.
pub(super) const MIGRATIONS: &[&str] = &[
    "CREATE TABLE kv (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE watch (
        target_id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        level TEXT NOT NULL
    );
    CREATE TABLE snooze (
        channel_id TEXT PRIMARY KEY,
        until INTEGER NOT NULL
    );
    CREATE TABLE category (
        target_id TEXT PRIMARY KEY,
        category TEXT NOT NULL
    );
    CREATE TABLE voice_profile (
        category TEXT PRIMARY KEY,
        body TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE chat_notes (
        channel_id TEXT PRIMARY KEY,
        body TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE own_message (
        message_id TEXT PRIMARY KEY,
        channel_id TEXT NOT NULL,
        guild_id TEXT,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE opportunity (
        message_id TEXT PRIMARY KEY,
        channel_id TEXT NOT NULL,
        guild_id TEXT NOT NULL,
        rule TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        status TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE activity (
        message_id TEXT PRIMARY KEY,
        guild_id TEXT NOT NULL,
        channel_id TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        kind TEXT NOT NULL,
        is_reply INTEGER NOT NULL,
        via_tritone INTEGER NOT NULL
    );",
    // v2: an older server's default channel has the server's own id, so
    // watch levels are keyed by (id, kind).
    "CREATE TABLE watch_v2 (
        target_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        level TEXT NOT NULL,
        PRIMARY KEY (target_id, kind)
    );
    INSERT INTO watch_v2 (target_id, kind, level) SELECT target_id, kind, level FROM watch;
    DROP TABLE watch;
    ALTER TABLE watch_v2 RENAME TO watch;",
];

pub fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        conn.execute_batch(&format!("BEGIN; {sql} PRAGMA user_version = {}; COMMIT;", i + 1))?;
    }
    Ok(())
}
