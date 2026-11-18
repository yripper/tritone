use super::*;
use crate::discord::types::Id;

fn user_version(s: &Store) -> i64 {
    s.conn().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap()
}

#[test]
fn migrations_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.db");
    assert_eq!(user_version(&Store::open(&path).unwrap()), 1);
    let s = Store::open(&path).unwrap();
    assert_eq!(user_version(&s), 1);
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
    assert_eq!(s.get_watch(Id(1)).unwrap(), None);
    s.set_watch(Id(1), TargetKind::Guild, Some(WatchLevel::Muted)).unwrap();
    s.set_watch(Id(101), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    s.set_watch(Id(101), TargetKind::Channel, Some(WatchLevel::Normal)).unwrap();
    assert_eq!(s.get_watch(Id(1)).unwrap(), Some(WatchLevel::Muted));
    assert_eq!(s.get_watch(Id(101)).unwrap(), Some(WatchLevel::Normal));
    let all = s.all_watch().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[&Id(1)], WatchLevel::Muted);

    s.set_watch(Id(1), TargetKind::Guild, None).unwrap();
    assert_eq!(s.get_watch(Id(1)).unwrap(), None);
    assert_eq!(s.all_watch().unwrap().len(), 1);
    // Clearing what isn't set is fine.
    s.set_watch(Id(1), TargetKind::Guild, None).unwrap();
}

#[test]
fn watch_ids_are_decimal_strings() {
    let s = Store::open_in_memory().unwrap();
    s.set_watch(Id(u64::MAX), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    let raw: String = s.conn().query_row("SELECT target_id FROM watch", [], |r| r.get(0)).unwrap();
    assert_eq!(raw, u64::MAX.to_string());
    assert_eq!(s.get_watch(Id(u64::MAX)).unwrap(), Some(WatchLevel::Full));
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
