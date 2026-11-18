use serde_json::json;

use super::parse_message;
use crate::discord::types::Id;

fn base() -> serde_json::Value {
    json!({
        "id": "1001",
        "channel_id": "101",
        "author": {"id": "1", "username": "U001", "global_name": "User One", "bot": false},
        "content": "hello",
        "timestamp": "2026-01-02T03:04:05.678000+00:00",
        "edited_timestamp": null,
        "type": 0,
        "mentions": [],
        "mention_roles": [],
        "mention_everyone": false,
        "attachments": [],
        "embeds": []
    })
}

#[test]
fn parse_message_basic_fields() {
    let m = parse_message(&base()).expect("parses");
    assert_eq!(m.id, Id(1001));
    assert_eq!(m.channel_id, Id(101));
    assert_eq!(m.guild_id, None);
    assert_eq!(m.author.id, Id(1));
    assert_eq!(m.author.username, "U001");
    assert_eq!(m.author.display_name.as_deref(), Some("User One"));
    assert_eq!(m.content, "hello");
    assert_eq!(m.timestamp_ms, 1_767_323_045_678);
    assert!(!m.edited);
    assert_eq!(m.kind, 0);
}

#[test]
fn parse_message_placeholders() {
    let mut v = base();
    v["attachments"] = json!([
        {"id": "5", "filename": "a.png", "content_type": "image/png", "size": 10},
        {"id": "6", "filename": "b.pdf", "content_type": "application/pdf", "size": 10},
        {"id": "7", "filename": "c.JPG", "size": 10}
    ]);
    v["embeds"] =
        json!([{"type": "rich", "title": "Release notes"}, {"type": "image", "url": "https://example.com/x.png"}]);
    v["reactions"] = json!([
        {"emoji": {"id": "77", "name": "party"}, "count": 3, "me": false},
        {"emoji": {"id": null, "name": "👍"}, "count": 2, "me": true}
    ]);
    let m = parse_message(&v).expect("parses");
    assert_eq!(m.attachments, vec!["image: a.png", "file: b.pdf", "image: c.JPG"]);
    assert_eq!(m.embeds, vec!["Release notes"]);
    assert_eq!(m.reactions, vec![(":party:".to_string(), 3), ("👍".to_string(), 2)]);
}

#[test]
fn parse_message_reply_mentions_and_gateway_fields() {
    let mut v = base();
    v["type"] = json!(19);
    v["guild_id"] = json!("11");
    v["edited_timestamp"] = json!("2026-01-02T03:05:00+00:00");
    v["message_reference"] = json!({"message_id": "900", "channel_id": "101"});
    v["referenced_message"] = json!({"id": "900", "author": {"id": "2", "username": "U002"}});
    v["mentions"] = json!([{"id": "2", "username": "U002"}]);
    v["mention_roles"] = json!(["31"]);
    v["mention_everyone"] = json!(true);
    let m = parse_message(&v).expect("parses");
    assert_eq!(m.kind, 19);
    assert_eq!(m.guild_id, Some(Id(11)));
    assert!(m.edited);
    assert_eq!(m.reply_to, Some(Id(900)));
    assert_eq!(m.reply_to_author, Some(Id(2)));
    assert_eq!(m.mentions, vec![Id(2)]);
    assert_eq!(m.mention_roles, vec![Id(31)]);
    assert!(m.mention_everyone);
}

#[test]
fn parse_message_tolerates_a_deleted_reference_and_missing_optionals() {
    let v = json!({
        "id": "1001",
        "channel_id": "101",
        "author": {"id": "1", "username": "U001"},
        "message_reference": {"message_id": "900"},
        "referenced_message": null
    });
    let m = parse_message(&v).expect("parses");
    assert_eq!(m.content, "");
    assert_eq!(m.reply_to, Some(Id(900)));
    assert_eq!(m.reply_to_author, None);
    // No timestamp: fall back to the id's.
    assert_eq!(m.timestamp_ms, Id(1001).timestamp_ms());
    assert!(m.attachments.is_empty() && m.reactions.is_empty());
}

#[test]
fn parse_message_rejects_what_is_not_a_message() {
    assert!(parse_message(&json!({})).is_none());
    assert!(parse_message(&json!({"id": "1", "channel_id": "2"})).is_none());
    assert!(parse_message(&json!({"id": "x", "channel_id": "2", "author": {"id": "1", "username": "U"}})).is_none());
    assert!(parse_message(&json!("text")).is_none());
}

#[test]
fn parse_user_reads_display_name_and_bot() {
    use super::parse_user;
    let u = parse_user(&json!({"id": "2", "username": "U002", "global_name": null, "bot": true})).unwrap();
    assert_eq!((u.id, u.username.as_str(), u.display_name, u.bot), (Id(2), "U002", None, true));
    assert!(parse_user(&json!({"id": "2"})).is_none());
}

#[test]
fn parse_channel_reads_threads_dms_and_overwrites() {
    use super::parse_channel;
    use crate::discord::types::ChannelKind;
    let t = parse_channel(&json!({
        "id": "201", "type": 11, "guild_id": "11", "parent_id": "101", "name": "T001",
        "last_message_id": "1005", "position": 3,
        "permission_overwrites": [
            {"id": "11", "type": 0, "allow": "1024", "deny": "0"},
            {"id": "1", "type": "member", "allow": "0", "deny": "2048"}
        ]
    }))
    .unwrap();
    assert_eq!(
        (t.id, t.kind, t.guild_id, t.parent_id),
        (Id(201), ChannelKind::PublicThread, Some(Id(11)), Some(Id(101)))
    );
    assert_eq!(t.last_message_id, Some(Id(1005)));
    assert_eq!(t.position, 3);
    assert_eq!(t.overwrites.len(), 2);
    assert!(!t.overwrites[0].is_member && t.overwrites[0].allow == 1024);
    assert!(t.overwrites[1].is_member && t.overwrites[1].deny == 2048);

    let dm = parse_channel(&json!({"id": "301", "type": 1, "recipients": [{"id": "2", "username": "U002"}]})).unwrap();
    assert_eq!(dm.kind, ChannelKind::Dm);
    assert_eq!(dm.recipients.len(), 1);
    assert!(parse_channel(&json!({"id": "301"})).is_none());
}
