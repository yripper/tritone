use super::*;
use crate::store::ActivityRow;

const NOW: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;

fn own(id: u64, channel: u64, guild: Option<u64>, at: i64) -> OwnMessageRow {
    OwnMessageRow { message_id: Id(id), channel_id: Id(channel), guild_id: guild.map(Id), created_at: at }
}

#[test]
fn profiles_and_notes_round_trip_and_replace() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.profile("work").unwrap(), None);
    s.save_profile("work", "short", NOW - DAY).unwrap();
    s.save_profile("work", "shorter", NOW).unwrap();
    s.save_profile("friends", "casual", NOW).unwrap();
    assert_eq!(s.profile("work").unwrap(), Some(VoiceEntry { body: "shorter".into(), updated_at: NOW }));
    assert_eq!(s.profile_dates().unwrap(), [("friends".to_string(), NOW), ("work".to_string(), NOW)]);

    s.save_notes(Id(100), "terse", NOW).unwrap();
    assert_eq!(s.notes(Id(100)).unwrap().map(|n| n.body), Some("terse".into()));
    assert_eq!(s.notes(Id(101)).unwrap(), None);
}

#[test]
fn categories_set_replace_and_list() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.category(Id(10)).unwrap(), None);
    s.set_category(Id(10), "work").unwrap();
    s.set_category(Id(100), "friends").unwrap();
    s.set_category(Id(10), "dev-community").unwrap();
    assert_eq!(s.category(Id(10)).unwrap().as_deref(), Some("dev-community"));
    assert_eq!(s.targets_in_category("dev-community").unwrap(), [Id(10)]);
}

#[test]
fn own_messages_count_once_skip_tritone_sends_and_prune_old_rows() {
    let s = Store::open_in_memory().unwrap();
    let rows = [own(1, 100, Some(10), NOW - 2 * DAY), own(2, 100, Some(10), NOW - DAY), own(3, 200, None, NOW - DAY)];
    s.record_own_messages(&rows, NOW).unwrap();
    s.record_own_messages(&rows[..1], NOW).unwrap();
    assert_eq!(s.own_count_in_channel(Id(100), NOW - 3 * DAY).unwrap(), 2);
    assert_eq!(s.own_count_in_channel(Id(100), NOW - 2 * DAY).unwrap(), 1, "strictly after");

    // Sent through tritone: Claude's words, not the user's voice.
    let sent = ActivityRow {
        message_id: Id(2),
        guild_id: Id(10),
        channel_id: Id(100),
        created_at: NOW - DAY,
        kind: "other".into(),
        is_reply: false,
        via_tritone: true,
    };
    s.record_activity(&sent).unwrap();
    assert_eq!(s.own_count_in_channel(Id(100), NOW - 3 * DAY).unwrap(), 1);
    let since: Vec<Id> = s.own_since(NOW - 3 * DAY).unwrap().into_iter().map(|(c, _)| c).collect();
    assert_eq!(since.len(), 2);
    assert!(since.contains(&Id(100)) && since.contains(&Id(200)));

    s.record_own_messages(&[own(4, 300, None, NOW - 30 * DAY)], NOW).unwrap();
    assert_eq!(s.own_count_in_channel(Id(300), 0).unwrap(), 0, "older than the keep window is pruned");
    assert_eq!(s.sent_via_tritone(&[Id(1), Id(2)]).unwrap(), [Id(2)].into_iter().collect());
}

#[test]
fn recent_own_channels_merge_both_logs_newest_first() {
    let s = Store::open_in_memory().unwrap();
    s.record_own_messages(&[own(1, 100, Some(10), NOW - 3 * DAY), own(2, 200, None, NOW - DAY)], NOW).unwrap();
    let row = ActivityRow {
        message_id: Id(3),
        guild_id: Id(20),
        channel_id: Id(300),
        created_at: NOW - 2 * DAY,
        kind: "other".into(),
        is_reply: false,
        via_tritone: false,
    };
    s.record_activity(&row).unwrap();
    assert_eq!(s.recent_own_channels(10).unwrap(), [(Id(200), None), (Id(300), Some(Id(20))), (Id(100), Some(Id(10)))]);
}

#[test]
fn last_post_and_channels_per_guild_come_from_both_logs() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.last_own_post_in_guild(Id(10)).unwrap(), None);
    s.record_own_messages(&[own(1, 100, Some(10), NOW - 3 * DAY), own(2, 200, None, NOW)], NOW).unwrap();
    let row = ActivityRow {
        message_id: Id(3),
        guild_id: Id(10),
        channel_id: Id(101),
        created_at: NOW - DAY,
        kind: "other".into(),
        is_reply: false,
        via_tritone: false,
    };
    s.record_activity(&row).unwrap();
    assert_eq!(s.last_own_post_in_guild(Id(10)).unwrap(), Some(NOW - DAY));
    assert_eq!(s.own_channels_in_guild(Id(10)).unwrap(), [Id(100), Id(101)].into_iter().collect());
}
