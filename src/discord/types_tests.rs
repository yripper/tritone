use super::*;

#[test]
fn id_serde_is_a_decimal_string() {
    let id = Id(u64::MAX);
    let s = serde_json::to_string(&id).unwrap();
    assert_eq!(s, format!("\"{}\"", u64::MAX));
    assert_eq!(serde_json::from_str::<Id>(&s).unwrap(), id);
    assert_eq!(serde_json::from_str::<Id>("12").unwrap(), Id(12), "numbers are accepted too");
    assert!(serde_json::from_str::<Id>("\"x\"").is_err());
    assert_eq!("42".parse::<Id>().unwrap(), Id(42));
    assert_eq!(Id(42).to_string(), "42");
}

#[test]
fn id_timestamp_roundtrip() {
    let ms = 1_800_000_000_123;
    let id = Id::from_timestamp_ms(ms, 5);
    assert_eq!(id.timestamp_ms(), ms);
    assert!(Id::from_timestamp_ms(ms, 6) > id);
    assert_eq!(Id(0).timestamp_ms(), 1_420_070_400_000);
}

#[test]
fn channel_kind_from_u8() {
    let cases = [
        (0, ChannelKind::Text),
        (1, ChannelKind::Dm),
        (2, ChannelKind::Voice),
        (3, ChannelKind::GroupDm),
        (4, ChannelKind::Category),
        (5, ChannelKind::Announcement),
        (10, ChannelKind::AnnouncementThread),
        (11, ChannelKind::PublicThread),
        (12, ChannelKind::PrivateThread),
        (15, ChannelKind::Forum),
        (16, ChannelKind::Media),
        (13, ChannelKind::Other(13)),
    ];
    for (n, k) in cases {
        assert_eq!(ChannelKind::from_u8(n), k);
    }
}

#[test]
fn user_label_prefers_display_name() {
    let mut u = UserRef { id: Id(1), username: "u001".into(), display_name: None, bot: false };
    assert_eq!(u.label(), "u001");
    u.display_name = Some("U One".into());
    assert_eq!(u.label(), "U One");
}

#[test]
fn halting_errors() {
    assert!(DiscordError::NeedsLogin.halts());
    assert!(DiscordError::Captcha.halts());
    assert!(DiscordError::Locked.halts());
    assert!(DiscordError::Blocked { retry_after_s: 60 }.halts());
    assert!(!DiscordError::RateLimited { retry_after_s: 1 }.halts());
    assert!(!DiscordError::Indexing { retry_after_s: 1 }.halts());
    assert!(!DiscordError::Forbidden.halts());
    assert!(!DiscordError::Http("x".into()).halts());
    assert!(DiscordError::Captcha.to_string().contains("official app"));
}

#[test]
fn gateway_closed_halts_and_says_restart() {
    let e = DiscordError::GatewayClosed { code: 4011 };
    assert!(e.halts());
    assert_eq!(e.kind(), "gateway_closed");
    assert_eq!(e.to_string(), "Discord closed the session (code 4011): restart tritone (pm2 restart tritone)");
}
