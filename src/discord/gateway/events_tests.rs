use serde_json::json;

use super::*;
use crate::discord::types::{ChannelKind, MuteUntil};

const ME: Id = Id(1);

fn one(t: &str, d: Value) -> GatewayEvent {
    let mut evs = parse_dispatch(t, &d, Some(ME));
    assert_eq!(evs.len(), 1, "{t} gives one event");
    evs.remove(0)
}

#[test]
fn message_create_and_ack() {
    let d = json!({
        "id": "5000", "channel_id": "100", "guild_id": "10", "type": 0, "content": "hi",
        "timestamp": "2026-01-01T00:00:00+00:00", "edited_timestamp": null,
        "author": {"id": "2", "username": "U002"}, "mentions": [], "mention_roles": [],
        "mention_everyone": false, "attachments": [], "embeds": []
    });
    assert!(matches!(one("MESSAGE_CREATE", d), GatewayEvent::MessageCreate(m) if m.id == Id(5000)));
    let ack = one("MESSAGE_ACK", json!({"channel_id": "100", "message_id": "5000", "mention_count": 0, "version": 7}));
    assert!(matches!(ack, GatewayEvent::MessageAck { channel: Id(100), message: Id(5000), mention_count: Some(0) }));
    let no_count = one("MESSAGE_ACK", json!({"channel_id": "100", "message_id": "5001"}));
    assert!(matches!(no_count, GatewayEvent::MessageAck { mention_count: None, .. }));
}

#[test]
fn channel_events() {
    let up = one("CHANNEL_UPDATE", json!({"id": "100", "type": 0, "guild_id": "10", "name": "C100"}));
    assert!(matches!(up, GatewayEvent::ChannelUpsert(c) if c.id == Id(100) && c.guild_id == Some(Id(10))));
    let dm = one("CHANNEL_CREATE", json!({"id": "300", "type": 1, "recipients": [{"id": "2", "username": "U002"}]}));
    assert!(matches!(dm, GatewayEvent::ChannelUpsert(c) if c.kind == ChannelKind::Dm && c.recipients[0].id == Id(2)));
    assert!(matches!(one("CHANNEL_DELETE", json!({"id": "100", "type": 0})), GatewayEvent::ChannelDelete(Id(100))));
}

#[test]
fn thread_events_become_channels() {
    let t = json!({"id": "110", "type": 11, "guild_id": "10", "parent_id": "100", "name": "T110"});
    for name in ["THREAD_CREATE", "THREAD_UPDATE"] {
        let ev = one(name, t.clone());
        assert!(
            matches!(ev, GatewayEvent::ChannelUpsert(c) if c.kind == ChannelKind::PublicThread && c.parent_id == Some(Id(100)))
        );
    }
    assert!(matches!(
        one("THREAD_DELETE", json!({"id": "110", "guild_id": "10", "type": 11})),
        GatewayEvent::ChannelDelete(Id(110))
    ));
}

#[test]
fn thread_list_sync_upserts_each_thread() {
    let d = json!({
        "guild_id": "10",
        "threads": [
            {"id": "110", "type": 11, "parent_id": "100"},
            {"id": "111", "type": 12, "parent_id": "100"}
        ],
        "members": []
    });
    let evs = parse_dispatch("THREAD_LIST_SYNC", &d, Some(ME));
    let ids: Vec<(Id, Option<Id>)> = evs
        .iter()
        .map(|e| match e {
            GatewayEvent::ChannelUpsert(c) => (c.id, c.guild_id),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![(Id(110), Some(Id(10))), (Id(111), Some(Id(10)))]);
}

#[test]
fn thread_list_sync_members_mark_joined() {
    // Op 37's `threads: true` syncs every active thread; `members` says
    // which ones the user is in.
    let d = json!({
        "guild_id": "10",
        "threads": [
            {"id": "110", "type": 11, "parent_id": "100"},
            {"id": "111", "type": 11, "parent_id": "100"}
        ],
        "members": [{"id": "110", "user_id": "1", "flags": 0}]
    });
    let joined: Vec<(Id, bool)> = parse_dispatch("THREAD_LIST_SYNC", &d, Some(ME))
        .into_iter()
        .map(|e| match e {
            GatewayEvent::ChannelUpsert(c) => (c.id, c.joined),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(joined, vec![(Id(110), true), (Id(111), false)]);
}

#[test]
fn thread_member_events_track_joined() {
    let t = json!({"id": "110", "type": 11, "guild_id": "10", "parent_id": "100", "name": "T110"});
    let mut added = t.clone();
    added["member"] = json!({"id": "110", "user_id": "1", "flags": 0});
    assert!(matches!(one("THREAD_CREATE", added), GatewayEvent::ChannelUpsert(c) if c.joined));
    let mut created = t.clone();
    created["newly_created"] = json!(true);
    assert!(matches!(one("THREAD_CREATE", created), GatewayEvent::ChannelUpsert(c) if !c.joined));

    let me = json!({"id": "110", "user_id": "1", "guild_id": "10", "flags": 0});
    let joined = |t: &str, d: Value| match one(t, d) {
        GatewayEvent::ThreadMembership { thread: Id(110), joined } => joined,
        other => panic!("unexpected {other:?}"),
    };
    assert!(joined("THREAD_MEMBER_UPDATE", me));
    let removed = json!({"id": "110", "guild_id": "10", "member_count": 3, "removed_member_ids": ["1"]});
    assert!(!joined("THREAD_MEMBERS_UPDATE", removed));
    let added_me =
        json!({"id": "110", "guild_id": "10", "member_count": 4, "added_members": [{"id": "110", "user_id": "1"}]});
    assert!(joined("THREAD_MEMBERS_UPDATE", added_me));
    let others = json!({"id": "110", "guild_id": "10", "member_count": 4,
        "added_members": [{"id": "110", "user_id": "2"}], "removed_member_ids": ["3"]});
    assert!(parse_dispatch("THREAD_MEMBERS_UPDATE", &others, Some(ME)).is_empty(), "other people's membership");
}

#[test]
fn guild_create_carries_channels_threads_and_my_roles() {
    let d = json!({
        "id": "10", "name": "G010", "owner_id": "999", "roles": [{"id": "11", "name": "helpers", "permissions": "1024"}],
        "channels": [{"id": "100", "type": 0}], "threads": [{"id": "110", "type": 11, "parent_id": "100"}],
        "members": [{"user": {"id": "1"}, "roles": ["11"]}, {"user": {"id": "2"}, "roles": ["12"]}]
    });
    match one("GUILD_CREATE", d) {
        GatewayEvent::GuildUpsert(g, chans) => {
            assert_eq!((g.id, g.name.as_str(), g.my_roles.clone()), (Id(10), "G010", vec![Id(11)]));
            assert_eq!(g.roles[0].name, "helpers");
            assert_eq!(chans.iter().map(|c| c.id).collect::<Vec<_>>(), vec![Id(100), Id(110)]);
            assert!(chans.iter().all(|c| c.guild_id == Some(Id(10))));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(parse_dispatch("GUILD_CREATE", &json!({"id": "10", "unavailable": true}), Some(ME)).is_empty());
}

#[test]
fn guild_update_delete_and_member_update() {
    let up = one("GUILD_UPDATE", json!({"id": "10", "name": "G010b", "owner_id": "999", "roles": []}));
    assert!(matches!(up, GatewayEvent::GuildUpdate(g) if g.name == "G010b"));
    assert!(matches!(one("GUILD_DELETE", json!({"id": "10"})), GatewayEvent::GuildDelete(Id(10))));
    // An outage is not a leave.
    assert!(parse_dispatch("GUILD_DELETE", &json!({"id": "10", "unavailable": true}), Some(ME)).is_empty());

    let mine = one("GUILD_MEMBER_UPDATE", json!({"guild_id": "10", "user": {"id": "1"}, "roles": ["11", "12"]}));
    assert!(matches!(mine, GatewayEvent::MyRolesUpdate { guild: Id(10), roles } if roles == vec![Id(11), Id(12)]));
    let other = json!({"guild_id": "10", "user": {"id": "2"}, "roles": ["11"]});
    assert!(parse_dispatch("GUILD_MEMBER_UPDATE", &other, Some(ME)).is_empty());
}

#[test]
fn guild_role_events() {
    let role = json!({"guild_id": "10", "role": {"id": "11", "name": "mods", "permissions": "1024"}});
    for name in ["GUILD_ROLE_CREATE", "GUILD_ROLE_UPDATE"] {
        let ev = one(name, role.clone());
        assert!(
            matches!(ev, GatewayEvent::RoleUpsert { guild: Id(10), role } if role.id == Id(11) && role.permissions == 1024 && role.name == "mods")
        );
    }
    let del = one("GUILD_ROLE_DELETE", json!({"guild_id": "10", "role_id": "11"}));
    assert!(matches!(del, GatewayEvent::RoleDelete { guild: Id(10), role: Id(11) }));
}

#[test]
fn settings_update() {
    let d = json!({"guild_id": "10", "muted": true, "mute_config": null, "channel_overrides": []});
    assert!(
        matches!(one("USER_GUILD_SETTINGS_UPDATE", d), GatewayEvent::SettingsUpdate(s) if s.muted == MuteUntil::Forever)
    );
}

#[test]
fn sessions_replace_and_unknown_events_are_ignored() {
    for name in ["SESSIONS_REPLACE", "PRESENCE_UPDATE", "TYPING_START", "SOMETHING_NEW"] {
        assert!(parse_dispatch(name, &json!([{"session_id": "x"}]), Some(ME)).is_empty(), "{name}");
    }
    // Malformed payloads give nothing rather than a half-built object.
    assert!(parse_dispatch("CHANNEL_UPDATE", &json!({"name": "no id"}), Some(ME)).is_empty());
}

#[test]
fn guild_create_never_takes_another_members_roles() {
    let d = json!({"id": "10", "name": "G010", "roles": [], "channels": [], "members": [{"user": {"id": "2"}, "roles": ["12"]}]});
    match one("GUILD_CREATE", d) {
        GatewayEvent::GuildUpsert(g, _) => assert!(g.my_roles.is_empty()),
        other => panic!("unexpected {other:?}"),
    }
}
