use super::*;

fn row(message: u64, created_at: i64) -> ActivityRow {
    ActivityRow {
        message_id: Id(message),
        guild_id: Id(10),
        channel_id: Id(100),
        created_at,
        kind: "other".into(),
        is_reply: false,
        via_tritone: false,
    }
}

#[test]
fn record_is_insert_or_ignore() {
    let s = Store::open_in_memory().unwrap();
    assert!(s.record_activity(&row(1, 1_000)).unwrap());
    let mut again = row(1, 1_000);
    again.kind = "answer".into();
    assert!(!s.record_activity(&again).unwrap(), "a second insert is ignored");
    assert_eq!(s.activity_between(0, 10_000).unwrap(), vec![row(1, 1_000)]);
}

#[test]
fn mark_sent_via_tritone_flips_an_existing_row_only() {
    let s = Store::open_in_memory().unwrap();
    assert!(!s.mark_sent_via_tritone(Id(1)).unwrap(), "no row yet");
    s.record_activity(&row(1, 1_000)).unwrap();
    assert!(s.mark_sent_via_tritone(Id(1)).unwrap());
    assert!(s.activity_between(0, 10_000).unwrap()[0].via_tritone);
}

#[test]
fn set_activity_kind_overrides() {
    let s = Store::open_in_memory().unwrap();
    s.record_activity(&row(1, 1_000)).unwrap();
    s.set_activity_kind(Id(1), "support").unwrap();
    assert_eq!(s.activity_between(0, 10_000).unwrap()[0].kind, "support");
}

#[test]
fn between_is_half_open_and_newest_first() {
    let s = Store::open_in_memory().unwrap();
    for (m, t) in [(1, 1_000), (2, 2_000), (3, 3_000)] {
        s.record_activity(&row(m, t)).unwrap();
    }
    let ids: Vec<Id> = s.activity_between(1_000, 3_000).unwrap().into_iter().map(|r| r.message_id).collect();
    assert_eq!(ids, vec![Id(2), Id(1)]);
}
