use std::collections::HashSet;

use super::*;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{Channel, DiscordError, Id, MuteUntil, Overwrite, Role};
use crate::test_support::*;

const ME: u64 = 7;
const NOW: i64 = 1_800_000_000_000;

fn live() -> LiveState {
    LiveState::new(BufferPolicy { per_channel: 3, max_age_ms: 3_600_000 })
}

/// G001 with text channels C101, C102 and a DM D201 with U002.
fn ready_live() -> LiveState {
    let l = live();
    let mut snap = ready(ME, vec![guild(1)], vec![text_channel(101, 1), text_channel(102, 1), dm(201, 2)]);
    snap.read_states.insert(Id(101), read_state(Some(id_at(NOW - 10_000)), 0));
    l.apply(GatewayEvent::Ready(Box::new(snap)), NOW);
    l
}

fn create(l: &LiveState, m: crate::discord::types::Message) {
    l.apply(GatewayEvent::MessageCreate(Box::new(m)), NOW);
}

#[test]
fn ready_populates_model() {
    let l = live();
    assert_eq!(l.connection(), ConnState::Connecting);
    let mut snap = ready(ME, vec![guild(1)], vec![text_channel(101, 1), dm(201, 2)]);
    snap.read_states.insert(Id(101), read_state(Some(Id(5)), 2));
    snap.settings.push(settings(Some(1), MuteUntil::Forever));
    // Settings for a guild the user left are ignored.
    snap.settings.push(settings(Some(99), MuteUntil::Forever));
    snap.users.push(user(3));
    let mut rx = l.subscribe();
    l.apply(GatewayEvent::Ready(Box::new(snap)), NOW);

    assert_eq!(l.connection(), ConnState::Ready { since_ms: NOW });
    assert_eq!(l.me().unwrap().username, "U007");
    assert_eq!(l.guilds().len(), 1);
    assert_eq!(l.guild(Id(1)).unwrap().name, "G001");
    assert_eq!(l.channels().len(), 2);
    assert_eq!(l.channel(Id(201)).unwrap().recipients[0].id, Id(2));
    assert_eq!(l.read_state(Id(101)), read_state(Some(Id(5)), 2));
    assert_eq!(l.read_state(Id(102)), ReadState::default());
    assert!(l.settings(Some(Id(1))).is_some());
    assert!(l.settings(Some(Id(99))).is_none());
    assert_eq!(l.user(Id(3)).unwrap().username, "U003");
    assert_eq!(l.user(Id(2)).unwrap().username, "U002", "DM recipients are known users");
    assert_eq!(l.user(Id(ME)).unwrap().username, "U007");
    assert!(matches!(rx.try_recv().unwrap(), LiveEvent::Ready));
}

#[test]
fn message_create_marks_unread_and_publishes() {
    let l = ready_live();
    assert!(!l.is_unread(Id(101)));
    let mut rx = l.subscribe();
    let id = id_at(NOW - 1_000);
    create(&l, msg(id, 101, 2, "hello"));

    assert!(l.is_unread(Id(101)));
    assert_eq!(l.channel(Id(101)).unwrap().last_message_id, Some(id));
    match rx.try_recv().unwrap() {
        LiveEvent::Message { msg, own } => {
            assert_eq!(msg.id, id);
            assert!(!own);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn message_create_in_unknown_dm_creates_the_channel() {
    let l = ready_live();
    create(&l, msg(id_at(NOW), 301, 4, "hi"));
    let c = l.channel(Id(301)).unwrap();
    assert_eq!(c.kind, crate::discord::types::ChannelKind::Dm);
    assert!(l.is_unread(Id(301)));
}

#[test]
fn mention_of_me_or_dm_bumps_mention_count() {
    let l = ready_live();
    let mut m = msg(id_at(NOW - 2_000), 101, 2, "hey");
    m.mentions = vec![Id(ME)];
    create(&l, m);
    create(&l, msg(id_at(NOW - 1_000), 101, 2, "no mention"));
    create(&l, msg(id_at(NOW - 500), 201, 2, "dm"));
    assert_eq!(l.read_state(Id(101)).mention_count, 1);
    assert_eq!(l.read_state(Id(201)).mention_count, 1);
}

#[test]
fn own_message_acks_channel() {
    let l = ready_live();
    let mut m = msg(id_at(NOW - 2_000), 101, 2, "ping");
    m.mentions = vec![Id(ME)];
    create(&l, m);
    assert!(l.is_unread(Id(101)));
    let mut rx = l.subscribe();
    let mine = id_at(NOW - 1_000);
    create(&l, msg(mine, 101, ME, "answer"));

    assert!(!l.is_unread(Id(101)));
    assert_eq!(l.read_state(Id(101)), read_state(Some(mine), 0));
    assert!(matches!(rx.try_recv().unwrap(), LiveEvent::Message { own: true, .. }));
}

#[test]
fn message_ack_clears_unread() {
    let l = ready_live();
    let id = id_at(NOW - 1_000);
    create(&l, msg(id, 101, 2, "x"));
    let mut rx = l.subscribe();
    l.apply(GatewayEvent::MessageAck { channel: Id(101), message: id, mention_count: Some(0) }, NOW);
    assert!(!l.is_unread(Id(101)));
    assert!(matches!(rx.try_recv().unwrap(), LiveEvent::Acked { channel: Id(101), message } if message == id));

    l.note_acked(Id(201), id_at(NOW));
    assert_eq!(l.read_state(Id(201)).last_acked, Some(id_at(NOW)));
}

#[test]
fn discord_mute_on_the_guild_mutes_every_channel() {
    // Discord ORs the guild, category and channel mutes: a channel override
    // with `muted:false` (common, from notification settings) doesn't unmute it.
    let l = ready_live();
    let mut s = settings(Some(1), MuteUntil::Forever);
    s.channel_mutes.insert(Id(102), MuteUntil::No);
    l.apply(GatewayEvent::SettingsUpdate(s), NOW);
    assert!(l.discord_muted(Id(101), NOW));
    assert!(l.discord_muted(Id(102), NOW));
    assert!(!l.discord_muted(Id(201), NOW), "DMs have no settings here");
}

#[test]
fn discord_mute_on_a_channel_or_category_mutes_it() {
    let l = ready_live();
    let mut cat = text_channel(301, 1);
    cat.kind = crate::discord::types::ChannelKind::Category;
    let mut under = text_channel(103, 1);
    under.parent_id = Some(Id(301));
    l.apply(GatewayEvent::ChannelUpsert(cat), NOW);
    l.apply(GatewayEvent::ChannelUpsert(under), NOW);
    let mut s = settings(Some(1), MuteUntil::No);
    s.channel_mutes.insert(Id(102), MuteUntil::Forever);
    s.channel_mutes.insert(Id(301), MuteUntil::Forever);
    s.channel_mutes.insert(Id(103), MuteUntil::No);
    l.apply(GatewayEvent::SettingsUpdate(s), NOW);
    assert!(l.discord_muted(Id(102), NOW));
    assert!(l.discord_muted(Id(103), NOW), "a muted category mutes its channels");
    assert!(!l.discord_muted(Id(101), NOW));
}

#[test]
fn thread_follows_parent_mute_override() {
    let l = ready_live();
    let mut thread = text_channel(401, 1);
    thread.kind = crate::discord::types::ChannelKind::PublicThread;
    thread.parent_id = Some(Id(102));
    l.apply(GatewayEvent::ChannelUpsert(thread), NOW);
    let mut s = settings(Some(1), MuteUntil::No);
    s.channel_mutes.insert(Id(102), MuteUntil::Forever);
    l.apply(GatewayEvent::SettingsUpdate(s), NOW);
    assert!(l.discord_muted(Id(401), NOW));
    assert!(!l.discord_muted(Id(101), NOW));
}

#[test]
fn thread_follows_its_parents_category_mute() {
    let l = ready_live();
    let mut cat = text_channel(301, 1);
    cat.kind = crate::discord::types::ChannelKind::Category;
    let mut parent = text_channel(103, 1);
    parent.parent_id = Some(Id(301));
    let mut thread = text_channel(401, 1);
    thread.kind = crate::discord::types::ChannelKind::PublicThread;
    thread.parent_id = Some(Id(103));
    for c in [cat, parent, thread] {
        l.apply(GatewayEvent::ChannelUpsert(c), NOW);
    }
    let mut s = settings(Some(1), MuteUntil::No);
    s.channel_mutes.insert(Id(301), MuteUntil::Forever);
    l.apply(GatewayEvent::SettingsUpdate(s), NOW);
    assert!(l.discord_muted(Id(401), NOW));
}

#[test]
fn mute_until_past_is_inactive() {
    assert!(!MuteUntil::No.active(NOW));
    assert!(MuteUntil::Forever.active(NOW));
    assert!(MuteUntil::Until(NOW + 1).active(NOW));
    assert!(!MuteUntil::Until(NOW - 1).active(NOW));

    let l = ready_live();
    l.apply(GatewayEvent::SettingsUpdate(settings(Some(1), MuteUntil::Until(NOW - 1))), NOW);
    assert!(!l.discord_muted(Id(101), NOW));
}

#[test]
fn can_view_respects_overwrites() {
    let l = live();
    let mut g = guild(1);
    g.roles.push(Role { id: Id(50), permissions: 0 });
    g.my_roles = vec![Id(50)];
    let hidden_from_everyone = vec![Overwrite { id: Id(1), is_member: false, allow: 0, deny: VIEW }];
    let mut c_role = text_channel(101, 1);
    c_role.overwrites = hidden_from_everyone.clone();
    c_role.overwrites.push(Overwrite { id: Id(50), is_member: false, allow: VIEW, deny: 0 });
    let mut c_hidden = text_channel(102, 1);
    c_hidden.overwrites = hidden_from_everyone.clone();
    let mut c_member_deny = text_channel(103, 1);
    c_member_deny.overwrites = vec![Overwrite { id: Id(ME), is_member: true, allow: 0, deny: VIEW }];
    let mut thread: Channel = text_channel(104, 1);
    thread.kind = crate::discord::types::ChannelKind::PublicThread;
    thread.parent_id = Some(Id(102));
    let channels = vec![c_role, c_hidden, c_member_deny, thread, text_channel(105, 1), dm(201, 2)];
    l.apply(GatewayEvent::Ready(Box::new(ready(ME, vec![g], channels))), NOW);

    assert!(l.can_view(Id(101)), "role allow beats @everyone deny");
    assert!(!l.can_view(Id(102)));
    assert!(!l.can_view(Id(103)), "member deny wins");
    assert!(!l.can_view(Id(104)), "threads follow their parent");
    assert!(l.can_view(Id(105)));
    assert!(l.can_view(Id(201)), "DMs are always visible");
    assert!(!l.can_view(Id(999)), "unknown channel");
}

#[test]
fn owner_and_admin_see_everything() {
    let l = live();
    let mut owned = guild(1);
    owned.owner_id = Id(ME);
    let mut admin = guild(2);
    admin.roles.push(Role { id: Id(60), permissions: 1 << 3 });
    admin.my_roles = vec![Id(60)];
    let mut a = text_channel(101, 1);
    a.overwrites = vec![Overwrite { id: Id(1), is_member: false, allow: 0, deny: VIEW }];
    let mut b = text_channel(102, 2);
    b.overwrites = vec![Overwrite { id: Id(2), is_member: false, allow: 0, deny: VIEW }];
    l.apply(GatewayEvent::Ready(Box::new(ready(ME, vec![owned, admin], vec![a, b]))), NOW);
    assert!(l.can_view(Id(101)));
    assert!(l.can_view(Id(102)));
}

#[test]
fn buffer_prunes_by_count_and_age() {
    let l = ready_live();
    l.set_buffered(HashSet::from([Id(101)]));
    // Older than max_age (1 h): dropped on the next insert.
    create(&l, msg(id_at(NOW - 2 * 3_600_000), 101, 2, "old"));
    for i in 0..4 {
        create(&l, msg(id_at(NOW - 1_000 + i), 101, 2, &format!("m{i}")));
    }
    let texts: Vec<String> = l.recent(Id(101)).into_iter().map(|m| m.content).collect();
    assert_eq!(texts, ["m1", "m2", "m3"], "oldest first, capped at per_channel");
}

#[test]
fn unbuffered_channel_keeps_no_text() {
    let l = ready_live();
    create(&l, msg(id_at(NOW - 1_000), 102, 2, "not kept"));
    assert!(l.recent(Id(102)).is_empty());
    // DMs are always buffered.
    create(&l, msg(id_at(NOW - 900), 201, 2, "kept"));
    assert_eq!(l.recent(Id(201)).len(), 1);
    // Un-buffering a channel drops what it held.
    l.set_buffered(HashSet::from([Id(101)]));
    create(&l, msg(id_at(NOW - 800), 101, 2, "a"));
    l.set_buffered(HashSet::new());
    assert!(l.recent(Id(101)).is_empty());
    assert_eq!(l.recent(Id(201)).len(), 1);
}

#[test]
fn disconnect_and_halt_update_connection() {
    let l = ready_live();
    l.apply(GatewayEvent::Disconnected, NOW);
    assert_eq!(l.connection(), ConnState::Reconnecting);
    l.apply(GatewayEvent::Resumed, NOW + 5);
    assert_eq!(l.connection(), ConnState::Ready { since_ms: NOW + 5 });
    l.apply(GatewayEvent::Halted(DiscordError::NeedsLogin), NOW);
    assert!(matches!(l.connection(), ConnState::Halted(_)));
}

#[test]
fn a_halt_survives_gateway_reconnects_until_cleared() {
    let l = ready_live();
    l.halt(DiscordError::Captcha);
    for ev in [GatewayEvent::Disconnected, GatewayEvent::Resumed] {
        l.apply(ev, NOW + 10);
        assert_eq!(l.connection(), ConnState::Halted(DiscordError::Captcha));
    }
    l.apply(GatewayEvent::Ready(Box::new(ready(ME, vec![guild(1)], vec![]))), NOW + 20);
    assert_eq!(l.connection(), ConnState::Halted(DiscordError::Captcha), "routine reconnects never lift a halt");
    l.clear_halt();
    assert_eq!(l.connection(), ConnState::Ready { since_ms: NOW + 20 });
}

#[test]
fn a_rejected_token_is_never_tried_again() {
    let l = ready_live();
    l.reject_token("tok-old");
    assert_eq!(l.connection(), ConnState::Halted(DiscordError::NeedsLogin));
    assert!(!l.try_login_with("tok-old"), "the same dead token must not wake the gateway");
    assert_eq!(l.connection(), ConnState::Halted(DiscordError::NeedsLogin));
    assert!(l.try_login_with("tok-new"));
    assert_eq!(l.connection(), ConnState::Connecting, "a new login lifts the halt");
}

#[test]
fn a_stale_token_cannot_reject_a_newer_login() {
    // The old REST client (or a late 4004) answering 401 with the old token
    // after a new login was accepted must not halt the new session.
    let l = ready_live();
    l.reject_token("tok-old");
    assert!(l.try_login_with("tok-new"));
    l.reject_token("tok-old");
    assert_eq!(l.connection(), ConnState::Connecting);
    assert!(!l.try_login_with("tok-new"), "the new login stays the one already tried");
    // The current token can still be rejected.
    l.reject_token("tok-new");
    assert_eq!(l.connection(), ConnState::Halted(DiscordError::NeedsLogin));
}

#[test]
fn a_token_tried_once_while_halted_is_not_tried_twice() {
    // Even if the gateway halts without naming the token, waking it with the
    // same token twice would loop identifies.
    let l = ready_live();
    l.apply(GatewayEvent::Halted(DiscordError::NeedsLogin), NOW);
    assert!(l.try_login_with("tok-1"));
    l.apply(GatewayEvent::Halted(DiscordError::NeedsLogin), NOW);
    assert!(!l.try_login_with("tok-1"));
    assert!(l.try_login_with("tok-2"));
}

#[test]
fn guild_update_keeps_channels_and_my_roles() {
    let l = ready_live();
    let mut g = guild(1);
    g.my_roles = vec![Id(900)];
    l.apply(GatewayEvent::GuildUpsert(Box::new(g), vec![text_channel(101, 1), text_channel(102, 1)]), NOW);
    let id = id_at(NOW);
    create(&l, msg(id, 101, 2, "x"));
    let mut renamed = guild(1);
    renamed.name = "G001-renamed".into();
    renamed.my_roles = vec![];
    l.apply(GatewayEvent::GuildUpdate(Box::new(renamed)), NOW);
    let g = l.guild(Id(1)).unwrap();
    assert_eq!(g.name, "G001-renamed");
    assert_eq!(g.my_roles, vec![Id(900)], "GUILD_UPDATE carries no member roles");
    assert!(l.channel(Id(102)).is_some(), "GUILD_UPDATE carries no channels");
    assert!(l.is_unread(Id(101)));
}

#[test]
fn my_roles_update_changes_visibility() {
    let l = ready_live();
    let mut g = guild(1);
    g.roles.push(Role { id: Id(900), permissions: 0 });
    l.apply(GatewayEvent::GuildUpdate(Box::new(g)), NOW);
    let mut c = text_channel(101, 1);
    c.overwrites = vec![
        Overwrite { id: Id(1), is_member: false, allow: 0, deny: VIEW },
        Overwrite { id: Id(900), is_member: false, allow: VIEW, deny: 0 },
    ];
    l.apply(GatewayEvent::ChannelUpsert(c), NOW);
    assert!(!l.can_view(Id(101)));
    l.apply(GatewayEvent::MyRolesUpdate { guild: Id(1), roles: vec![Id(900)] }, NOW);
    assert!(l.can_view(Id(101)));
    l.apply(GatewayEvent::MyRolesUpdate { guild: Id(99), roles: vec![] }, NOW);
}

#[test]
fn role_and_everyone_mentions_count_unless_suppressed() {
    let l = ready_live();
    l.apply(GatewayEvent::MyRolesUpdate { guild: Id(1), roles: vec![Id(900)] }, NOW);
    let mut role = msg(id_at(NOW - 3_000), 101, 2, "role ping");
    role.mention_roles = vec![Id(900)];
    let mut other_role = msg(id_at(NOW - 2_500), 101, 2, "other role");
    other_role.mention_roles = vec![Id(901)];
    let mut everyone = msg(id_at(NOW - 2_000), 101, 2, "everyone");
    everyone.mention_everyone = true;
    for m in [role.clone(), other_role, everyone.clone()] {
        create(&l, m);
    }
    assert_eq!(l.read_state(Id(101)).mention_count, 2);

    let mut s = settings(Some(1), MuteUntil::No);
    s.suppress_roles = true;
    s.suppress_everyone = true;
    l.apply(GatewayEvent::SettingsUpdate(s), NOW);
    role.channel_id = Id(102);
    everyone.channel_id = Id(102);
    create(&l, role);
    create(&l, everyone);
    assert_eq!(l.read_state(Id(102)).mention_count, 0, "suppressed in this server");
}

#[test]
fn a_stale_channel_update_never_moves_last_message_back() {
    let l = ready_live();
    let new = id_at(NOW);
    create(&l, msg(new, 101, 2, "x"));
    let mut stale = text_channel(101, 1);
    stale.last_message_id = Some(id_at(NOW - 60_000));
    l.apply(GatewayEvent::ChannelUpsert(stale), NOW);
    assert_eq!(l.channel(Id(101)).unwrap().last_message_id, Some(new));
    assert!(l.is_unread(Id(101)));
}

#[test]
fn guild_and_channel_lifecycle() {
    let l = ready_live();
    l.apply(GatewayEvent::GuildUpsert(Box::new(guild(2)), vec![text_channel(501, 2)]), NOW);
    assert!(l.guild(Id(2)).is_some());
    assert!(l.channel(Id(501)).is_some());
    l.apply(GatewayEvent::ChannelDelete(Id(501)), NOW);
    assert!(l.channel(Id(501)).is_none());
    l.apply(GatewayEvent::GuildUpsert(Box::new(guild(2)), vec![text_channel(502, 2)]), NOW);
    l.apply(GatewayEvent::GuildDelete(Id(2)), NOW);
    assert!(l.guild(Id(2)).is_none());
    assert!(l.channel(Id(502)).is_none(), "a deleted guild takes its channels");
    // An upsert without last_message_id keeps the known one.
    let id = id_at(NOW);
    create(&l, msg(id, 101, 2, "x"));
    l.apply(GatewayEvent::ChannelUpsert(text_channel(101, 1)), NOW);
    assert_eq!(l.channel(Id(101)).unwrap().last_message_id, Some(id));
}
