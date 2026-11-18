use serde_json::json;

use super::*;
use crate::discord::types::{ChannelKind, MuteUntil};

/// A READY in the shape capabilities 253 produce, with fake ids.
fn ready_json(read_state: Value) -> Value {
    json!({
        "v": 9,
        "user": {"id": "1", "username": "U001", "global_name": "Me", "bot": false},
        "session_id": "sess",
        "resume_gateway_url": "wss://gateway-test.discord.gg",
        "guilds": [
            {
                "id": "10", "name": "G010", "owner_id": "999", "member_count": 40, "large": false,
                "roles": [{"id": "10", "permissions": "1024"}, {"id": "11", "permissions": "8"}],
                "channels": [
                    {"id": "100", "type": 0, "name": "C100", "position": 2, "parent_id": "105",
                     "last_message_id": "5000",
                     "permission_overwrites": [
                        {"id": "10", "type": 0, "allow": "0", "deny": "1024"},
                        {"id": "1", "type": "member", "allow": "1024", "deny": "0"}
                     ]},
                    {"id": "105", "type": 4, "name": "cat", "position": 0}
                ],
                "threads": [{"id": "110", "type": 11, "name": "T110", "parent_id": "100", "last_message_id": "5100"}]
            },
            {
                "id": "20",
                "properties": {"name": "G020", "owner_id": "1"},
                "roles": [{"id": "20", "permissions": "1024"}],
                "channels": [{"id": "200", "type": 5, "name": "C200"}],
                "member_count": 5000, "large": true
            }
        ],
        "merged_members": [
            [{"user_id": "1", "roles": ["11"]}],
            [{"user_id": "1", "roles": []}]
        ],
        "private_channels": [
            {"id": "300", "type": 1, "recipient_ids": ["2"], "last_message_id": "7000"},
            {"id": "301", "type": 3, "name": "group", "recipient_ids": ["2", "3"]}
        ],
        "users": [
            {"id": "2", "username": "U002", "global_name": null},
            {"id": "3", "username": "U003", "global_name": "Three", "bot": true}
        ],
        "read_state": read_state,
        "user_guild_settings": {
            "entries": [
                {"guild_id": "10", "muted": false, "suppress_everyone": true, "suppress_roles": false,
                 "channel_overrides": [
                    {"channel_id": "100", "muted": true, "mute_config": null},
                    {"channel_id": "105", "muted": false}
                 ]},
                {"guild_id": "20", "muted": true, "mute_config": {"end_time": "2030-01-01T00:00:00+00:00"}},
                {"guild_id": "99", "muted": true},
                {"guild_id": null, "muted": false}
            ],
            "partial": false, "version": 3
        }
    })
}

fn entries() -> Value {
    json!([
        {"id": "100", "last_message_id": "4000", "mention_count": 2, "read_state_type": 0},
        {"id": "300", "last_message_id": "7000", "mention_count": 0},
        {"id": "10", "last_acked_id": "1", "badge_count": 4, "read_state_type": 1}
    ])
}

#[test]
fn parse_ready_versioned_and_bare_read_state() {
    let versioned = parse_ready(&ready_json(json!({"entries": entries(), "partial": false, "version": 9})), None);
    let bare = parse_ready(&ready_json(entries()), None);
    assert_eq!(versioned.read_states, bare.read_states);
    let rs = &versioned.read_states;
    assert_eq!(rs.len(), 2, "only read_state_type 0 is kept");
    assert_eq!(rs[&Id(100)], ReadState { last_acked: Some(Id(4000)), mention_count: 2 });
    assert_eq!(rs[&Id(300)].last_acked, Some(Id(7000)));
    assert!(!rs.contains_key(&Id(10)));
}

#[test]
fn parse_ready_core_fields() {
    let snap = parse_ready(&ready_json(entries()), None);
    assert_eq!(snap.me.id, Id(1));
    assert_eq!(snap.me.display_name.as_deref(), Some("Me"));
    assert_eq!(snap.session_id, "sess");
    assert_eq!(snap.resume_url, "wss://gateway-test.discord.gg");

    let g10 = snap.guilds.iter().find(|g| g.id == Id(10)).unwrap();
    assert_eq!((g10.name.as_str(), g10.owner_id, g10.member_count), ("G010", Id(999), Some(40)));
    assert_eq!(g10.roles.len(), 2);
    assert_eq!(g10.roles[1].permissions, 8);
    assert_eq!(g10.my_roles, vec![Id(11)], "my_roles come from merged_members, index-aligned");
    let g20 = snap.guilds.iter().find(|g| g.id == Id(20)).unwrap();
    assert_eq!((g20.name.as_str(), g20.owner_id, g20.large), ("G020", Id(1), true), "nested properties work too");

    let c100 = snap.channels.iter().find(|c| c.id == Id(100)).unwrap();
    assert_eq!(c100.guild_id, Some(Id(10)));
    assert_eq!(c100.parent_id, Some(Id(105)));
    assert_eq!(c100.kind, ChannelKind::Text);
    assert_eq!(c100.last_message_id, Some(Id(5000)));
    assert_eq!(c100.position, 2);
    assert_eq!(c100.overwrites.len(), 2);
    assert!(!c100.overwrites[0].is_member && c100.overwrites[0].deny == 1024);
    assert!(c100.overwrites[1].is_member && c100.overwrites[1].allow == 1024);

    let t = snap.channels.iter().find(|c| c.id == Id(110)).unwrap();
    assert_eq!((t.kind, t.guild_id, t.parent_id), (ChannelKind::PublicThread, Some(Id(10)), Some(Id(100))));
    assert!(t.joined, "READY only carries threads the user is in");
    assert!(!c100.joined);
    assert_eq!(snap.channels.iter().find(|c| c.id == Id(200)).unwrap().kind, ChannelKind::Announcement);
}

#[test]
fn parse_ready_dm_recipient_ids() {
    let snap = parse_ready(&ready_json(entries()), None);
    let dm = snap.channels.iter().find(|c| c.id == Id(300)).unwrap();
    assert_eq!(dm.kind, ChannelKind::Dm);
    assert_eq!(dm.guild_id, None);
    assert_eq!(dm.recipients.len(), 1);
    assert_eq!(dm.recipients[0].username, "U002");
    let group = snap.channels.iter().find(|c| c.id == Id(301)).unwrap();
    assert_eq!(group.kind, ChannelKind::GroupDm);
    let names: Vec<&str> = group.recipients.iter().map(|u| u.label()).collect();
    assert_eq!(names, vec!["U002", "Three"]);
    assert!(group.recipients[1].bot);
    assert_eq!(snap.users.len(), 2);
}

#[test]
fn parse_ready_drops_settings_for_left_guilds() {
    let snap = parse_ready(&ready_json(entries()), None);
    let ids: Vec<Option<Id>> = snap.settings.iter().map(|s| s.guild_id).collect();
    assert_eq!(ids.len(), 3);
    assert!(!ids.contains(&Some(Id(99))));
    assert!(ids.contains(&None), "DM settings stay");

    let g10 = snap.settings.iter().find(|s| s.guild_id == Some(Id(10))).unwrap();
    assert!(g10.suppress_everyone && !g10.suppress_roles);
    assert_eq!(g10.muted, MuteUntil::No);
    assert_eq!(g10.channel_mutes[&Id(100)], MuteUntil::Forever);
    assert_eq!(g10.channel_mutes[&Id(105)], MuteUntil::No);
    let g20 = snap.settings.iter().find(|s| s.guild_id == Some(Id(20))).unwrap();
    assert_eq!(g20.muted, MuteUntil::Until(1_893_456_000_000));
}

#[test]
fn parse_ready_takes_merged_members_from_supplemental() {
    let mut ready = ready_json(entries());
    ready.as_object_mut().unwrap().remove("merged_members");
    let supp = json!({
        "guilds": [{"id": "10"}, {"id": "20"}],
        "merged_members": [[{"user_id": "1", "roles": ["11"]}], [{"user_id": "1", "roles": ["21"]}]]
    });
    let snap = parse_ready(&ready, Some(&supp));
    let roles: Vec<Vec<Id>> = snap.guilds.iter().map(|g| g.my_roles.clone()).collect();
    assert_eq!(roles, vec![vec![Id(11)], vec![Id(21)]]);

    let evs = supplemental_roles(&supp, Id(1));
    assert_eq!(evs.len(), 2);
    assert!(
        matches!(&evs[1], GatewayEvent::MyRolesUpdate { guild, roles } if *guild == Id(20) && roles == &vec![Id(21)])
    );
}

#[test]
fn parse_ready_tolerates_missing_sections() {
    let snap = parse_ready(&json!({"user": {"id": "1", "username": "U001"}, "session_id": "s"}), None);
    assert_eq!(snap.me.id, Id(1));
    assert!(snap.guilds.is_empty() && snap.channels.is_empty() && snap.read_states.is_empty());
}

#[test]
fn supplemental_members_of_others_never_become_my_roles() {
    // With PRIORITIZED_READY_PAYLOAD, READY_SUPPLEMENTAL's merged_members are
    // other users' members (https://docs.discord.food/gateway/gateway-events#ready, note 7).
    let supp = json!({
        "guilds": [{"id": "10"}, {"id": "20"}],
        "merged_members": [[{"user_id": "2", "roles": ["12"]}], [{"user_id": "2", "roles": ["22"]}, {"user_id": "3", "roles": []}]]
    });
    assert!(supplemental_roles(&supp, Id(1)).is_empty());
    let mut ready = ready_json(entries());
    ready.as_object_mut().unwrap().remove("merged_members");
    let snap = parse_ready(&ready, Some(&supp));
    assert!(snap.guilds.iter().all(|g| g.my_roles.is_empty()));
    // READY's own merged_members with a single other user's entry is not mine either.
    let mut ready = ready_json(entries());
    ready["merged_members"] = json!([[{"user_id": "2", "roles": ["11"]}], []]);
    assert!(parse_ready(&ready, None).guilds.iter().all(|g| g.my_roles.is_empty()));
}
