use super::*;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{Channel, ChannelKind, Id, Overwrite, UserRef};
use crate::domain::errors::Error;
use crate::domain::live::{BufferPolicy, LiveState};
use crate::test_support::*;

const ME: u64 = 7;
const NOW: i64 = 1_800_000_000_000;

fn named_channel(id: u64, guild: u64, name: &str) -> Channel {
    Channel { name: Some(name.into()), ..text_channel(id, guild) }
}

/// Twelve servers G001..G012, each with `#general` (id 100 + n). G002 is
/// called "Rust Lang". G001 also has `#general-help` (150), a hidden
/// `#secret` (151), a category called "general" (152) and a nameless channel
/// (153). DMs: D201 with U002, D202 with U003 ("Sam"), and a group DM D203.
fn world() -> LiveState {
    let l = LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 86_400_000 });
    let mut guilds: Vec<_> = (1..=12).map(guild).collect();
    guilds[1].name = "Rust Lang".into();
    let mut channels: Vec<Channel> = (1..=12).map(|n| named_channel(100 + n, n, "general")).collect();
    channels.push(named_channel(150, 1, "general-help"));
    let mut secret = named_channel(151, 1, "secret");
    secret.overwrites = vec![Overwrite { id: Id(1), is_member: false, allow: 0, deny: VIEW }];
    channels.push(secret);
    channels.push(Channel { kind: ChannelKind::Category, ..named_channel(152, 1, "general") });
    channels.push(Channel { name: None, ..text_channel(153, 1) });
    channels.push(dm(201, 2));
    let mut sam = dm(202, 3);
    sam.recipients = vec![UserRef { display_name: Some("Sam".into()), ..user(3) }];
    channels.push(sam);
    channels.push(Channel {
        kind: ChannelKind::GroupDm,
        name: Some("weekend".into()),
        recipients: vec![user(2), user(4)],
        ..dm(203, 2)
    });
    l.apply(GatewayEvent::Ready(Box::new(ready(ME, guilds, channels))), NOW);
    l
}

fn candidates(e: Error) -> Vec<String> {
    match e {
        Error::Ambiguous { candidates, .. } => candidates,
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

#[test]
fn general_in_many_servers_is_ambiguous() {
    let l = world();
    for q in ["#general", "general", "#GENERAL"] {
        let c = candidates(resolve_channel(q, &l).unwrap_err());
        assert_eq!(c.len(), 10, "at most 10 labels: {c:?}");
        assert!(c.iter().all(|s| s.contains(" › #general (")), "{c:?}");
        assert!(c.iter().all(|s| !s.contains("general-help")), "exact names win over substrings: {c:?}");
        assert_eq!(c[0], "G001 › #general (101)", "sorted labels with the id: {c:?}");
    }
}

#[test]
fn server_slash_channel_disambiguates() {
    let l = world();
    assert_eq!(resolve_channel("rust/#general", &l).unwrap(), Id(102));
    assert_eq!(resolve_channel("G001/#general", &l).unwrap(), Id(101));
    assert_eq!(resolve_channel("g001/#help", &l).unwrap(), Id(150));
    assert!(matches!(resolve_channel("nope/#general", &l), Err(Error::NotFound(_))));
}

#[test]
fn link_resolves() {
    let l = world();
    assert_eq!(resolve_channel("https://discord.com/channels/1/101/555", &l).unwrap(), Id(101));
    assert_eq!(resolve_channel("https://ptb.discord.com/channels/1/150", &l).unwrap(), Id(150));
    assert_eq!(resolve_channel("discord.com/channels/@me/201", &l).unwrap(), Id(201));
    assert_eq!(resolve_channel("  101 ", &l).unwrap(), Id(101));
    // A channel tritone hasn't seen (e.g. a thread it isn't in) goes through; Discord decides.
    assert_eq!(resolve_channel("4242", &l).unwrap(), Id(4242));
    assert!(matches!(resolve_channel("https://discord.com/channels/1/", &l), Err(Error::Invalid(_))));
}

#[test]
fn dm_by_user() {
    let l = world();
    assert_eq!(resolve_channel("@U002", &l).unwrap(), Id(201), "the 1:1 DM, not the group DM");
    assert_eq!(resolve_channel("@sam", &l).unwrap(), Id(202));
    assert_eq!(resolve_channel("@u003", &l).unwrap(), Id(202), "username works too");
    assert_eq!(resolve_channel("weekend", &l).unwrap(), Id(203), "group DMs by name");
    let c = candidates(resolve_channel("@U00", &l).unwrap_err());
    assert_eq!(c, vec!["@Sam (DM, 202)".to_string(), "@U002 (DM, 201)".to_string()]);
}

#[test]
fn hidden_channel_not_resolved() {
    let l = world();
    assert!(matches!(resolve_channel("#secret", &l), Err(Error::NotFound(_))));
    assert!(matches!(resolve_channel("151", &l), Err(Error::NotFound(_))));
    assert!(matches!(resolve_channel("https://discord.com/channels/1/151", &l), Err(Error::NotFound(_))));
}

#[test]
fn categories_and_nameless_channels_never_match() {
    let l = world();
    // The category "general" in G001 would make G001/#general ambiguous.
    assert_eq!(resolve_channel("G001/#general", &l).unwrap(), Id(101));
    assert!(matches!(resolve_channel("", &l), Err(Error::Invalid(_))));
    assert!(matches!(resolve_channel("#", &l), Err(Error::Invalid(_))));
}

#[test]
fn servers_resolve_by_id_or_name() {
    let l = world();
    assert_eq!(resolve_server("2", &l).unwrap(), Id(2));
    assert_eq!(resolve_server("rust", &l).unwrap(), Id(2));
    assert_eq!(resolve_server("G001", &l).unwrap(), Id(1), "exact beats G0010-style substrings");
    let c = candidates(resolve_server("G0", &l).unwrap_err());
    assert_eq!(c.len(), 10);
    assert_eq!(c[0], "G001 (1)");
    assert!(matches!(resolve_server("999", &l), Err(Error::NotFound(_))));
    assert!(matches!(resolve_server("nope", &l), Err(Error::NotFound(_))));
}

#[test]
fn users_resolve_by_id_or_unique_name() {
    let l = world();
    assert_eq!(resolve_user("3", &l).unwrap(), Id(3));
    assert_eq!(resolve_user("@sam", &l).unwrap(), Id(3));
    assert_eq!(resolve_user("u002", &l).unwrap(), Id(2));
    let c = candidates(resolve_user("U00", &l).unwrap_err());
    assert_eq!(c, vec!["Sam (@U003, 3)".to_string(), "U002 (@U002, 2)".to_string(), "U004 (@U004, 4)".to_string()]);
    assert!(matches!(resolve_user("zed", &l), Err(Error::NotFound(_))));
}
