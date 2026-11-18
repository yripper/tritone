use std::collections::HashMap;

use serde_json::json;

use super::*;
use crate::store::WatchLevel;
use crate::test_support::*;

const T: i64 = 1_800_000_000_000;

fn with_last(mut c: crate::discord::types::Channel, ms: i64) -> crate::discord::types::Channel {
    c.last_message_id = Some(id_at(ms));
    c
}

#[test]
fn missed_guilds_detects_gap() {
    let snap = ready(
        1,
        vec![guild(1), guild(2), guild(3)],
        vec![
            // G001: newer than anything seen live, and posted while connected: a miss.
            with_last(text_channel(101, 1), T - 5_000),
            // G002: nothing new.
            with_last(text_channel(201, 2), T - 20_000),
            // G003: newer, but posted while the socket was down: not a miss.
            with_last(text_channel(301, 3), T + 2_000),
            // A channel that didn't exist before the reconnect: no baseline.
            with_last(text_channel(302, 3), T - 5_000),
            // DMs always arrive; never counted.
            with_last(dm(401, 2), T - 5_000),
        ],
    );
    let seen: HashMap<Id, Id> = [
        (Id(101), id_at(T - 10_000)),
        (Id(201), id_at(T - 20_000)),
        (Id(301), id_at(T - 10_000)),
        (Id(401), id_at(T - 10_000)),
    ]
    .into();
    assert_eq!(missed_guilds(&snap, &seen, id_at(T)), vec![Id(1)]);
    assert!(missed_guilds(&snap, &HashMap::new(), id_at(T)).is_empty());
}

#[test]
fn subscription_packet_shape() {
    let p = subscription_packet(Id(10), &[Id(100), Id(101)]);
    assert_eq!(
        p,
        json!({"op": 37, "d": {"subscriptions": {"10": {
            "typing": true, "threads": true, "activities": true, "member_updates": false,
            "members": [], "channels": {"100": [[0, 99]], "101": [[0, 99]]}, "thread_member_lists": []
        }}}})
    );
}

#[test]
fn full_watch_channels_follow_overrides() {
    let mut thread = text_channel(103, 1);
    thread.kind = crate::discord::types::ChannelKind::PublicThread;
    thread.parent_id = Some(Id(101));
    let channels = vec![text_channel(101, 1), text_channel(102, 1), thread, text_channel(201, 2), text_channel(202, 2)];
    let watch: HashMap<Id, WatchLevel> =
        [(Id(1), WatchLevel::Full), (Id(102), WatchLevel::Normal), (Id(202), WatchLevel::Full)].into();
    let mut got: Vec<Id> = full_watch_channels(&channels, &watch).into_iter().collect();
    got.sort();
    // 101 and its thread via the guild, 102 overridden down, 202 on its own.
    assert_eq!(got, vec![Id(101), Id(103), Id(202)]);
}
