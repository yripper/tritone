use super::*;
use crate::discord::types::Id;

fn user_version(s: &Store) -> i64 {
    s.conn().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap()
}

#[test]
fn migrations_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    assert_eq!(user_version(&Store::open(&path).unwrap()), 2);
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 2);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn schema_v1_has_all_tables() {
    let s = Store::open_in_memory().unwrap();
    let conn = s.conn();
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").unwrap();
    let names: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(
        names,
        ["activity", "category", "chat_notes", "kv", "opportunity", "own_message", "snooze", "voice_profile", "watch"]
    );
}

#[test]
fn kv_roundtrip_and_overwrite() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.kv_get("build_number").unwrap(), None);
    s.kv_set("build_number", "1").unwrap();
    s.kv_set("build_number", "2").unwrap();
    assert_eq!(s.kv_get("build_number").unwrap().as_deref(), Some("2"));
}

#[test]
fn watch_set_clear_roundtrip() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.get_watch(Id(1), TargetKind::Guild).unwrap(), None);
    s.set_watch(Id(1), TargetKind::Guild, Some(WatchLevel::Muted)).unwrap();
    s.set_watch(Id(101), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    s.set_watch(Id(101), TargetKind::Channel, Some(WatchLevel::Normal)).unwrap();
    assert_eq!(s.get_watch(Id(1), TargetKind::Guild).unwrap(), Some(WatchLevel::Muted));
    assert_eq!(s.get_watch(Id(101), TargetKind::Channel).unwrap(), Some(WatchLevel::Normal));
    assert_eq!(s.get_watch(Id(101), TargetKind::Guild).unwrap(), None, "the kind is part of the key");
    let all = s.all_watch().unwrap();
    assert_eq!((all.guilds.len(), all.channels.len()), (1, 1));
    assert_eq!(all.guilds[&Id(1)], WatchLevel::Muted);

    s.set_watch(Id(1), TargetKind::Guild, None).unwrap();
    assert_eq!(s.get_watch(Id(1), TargetKind::Guild).unwrap(), None);
    assert!(s.all_watch().unwrap().guilds.is_empty());
    // Clearing what isn't set is fine.
    s.set_watch(Id(1), TargetKind::Guild, None).unwrap();
}

#[test]
fn guild_and_default_channel_same_id_both_watchable() {
    // Older servers' default channel has the server's id.
    let s = Store::open_in_memory().unwrap();
    s.set_watch(Id(1), TargetKind::Guild, Some(WatchLevel::Full)).unwrap();
    s.set_watch(Id(1), TargetKind::Channel, Some(WatchLevel::Muted)).unwrap();
    assert_eq!(s.get_watch(Id(1), TargetKind::Guild).unwrap(), Some(WatchLevel::Full));
    assert_eq!(s.get_watch(Id(1), TargetKind::Channel).unwrap(), Some(WatchLevel::Muted));
    let all = s.all_watch().unwrap();
    assert_eq!(all.guilds[&Id(1)], WatchLevel::Full);
    assert_eq!(all.channels[&Id(1)], WatchLevel::Muted);
    s.set_watch(Id(1), TargetKind::Channel, None).unwrap();
    assert_eq!(s.get_watch(Id(1), TargetKind::Guild).unwrap(), Some(WatchLevel::Full), "clearing one keeps the other");
}

#[test]
fn migration_v2_keeps_rows() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!("BEGIN; {} PRAGMA user_version = 1; COMMIT;", schema::MIGRATIONS[0])).unwrap();
    conn.execute_batch(
        "INSERT INTO watch VALUES ('1', 'guild', 'muted'); INSERT INTO watch VALUES ('101', 'channel', 'full');",
    )
    .unwrap();
    schema::migrate(&conn).unwrap();
    let s = Store { conn: std::sync::Mutex::new(conn) };
    assert_eq!(user_version(&s), 2);
    assert_eq!(s.get_watch(Id(1), TargetKind::Guild).unwrap(), Some(WatchLevel::Muted));
    assert_eq!(s.get_watch(Id(101), TargetKind::Channel).unwrap(), Some(WatchLevel::Full));
    s.set_watch(Id(1), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    assert_eq!(s.get_watch(Id(1), TargetKind::Guild).unwrap(), Some(WatchLevel::Muted));
}

#[test]
fn watch_ids_are_decimal_strings() {
    let s = Store::open_in_memory().unwrap();
    s.set_watch(Id(u64::MAX), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    let raw: String = s.conn().query_row("SELECT target_id FROM watch", [], |r| r.get(0)).unwrap();
    assert_eq!(raw, u64::MAX.to_string());
    assert_eq!(s.get_watch(Id(u64::MAX), TargetKind::Channel).unwrap(), Some(WatchLevel::Full));
}

#[test]
fn snooze_roundtrip() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.snoozed_until(Id(101)).unwrap(), None);
    s.set_snooze(Id(101), Some(500)).unwrap();
    s.set_snooze(Id(101), Some(900)).unwrap();
    s.set_snooze(Id(102), Some(7)).unwrap();
    assert_eq!(s.snoozed_until(Id(101)).unwrap(), Some(900));
    s.set_snooze(Id(101), None).unwrap();
    assert_eq!(s.snoozed_until(Id(101)).unwrap(), None);
    assert_eq!(s.snoozed_until(Id(102)).unwrap(), Some(7));
}
