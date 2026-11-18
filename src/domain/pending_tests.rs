use super::*;
use crate::discord::fake::Call;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{Channel, ChannelKind, Id, Message, MuteUntil, ReadySnapshot, Role};
use crate::domain::test_world::{ME, NOW, unread, world};
use crate::store::{TargetKind, WatchLevel, WatchMap};
use crate::test_support::*;

/// G001 (my role 9) with C101, C102, C103 and a thread T104 under C101;
/// G002 with C111; a DM D301 with U002 and a group DM D302 "trip".
fn snap() -> ReadySnapshot {
    let mut g1 = guild(1);
    g1.roles.push(Role { id: Id(9), permissions: 0 });
    g1.my_roles = vec![Id(9)];
    let thread = Channel {
        kind: ChannelKind::PublicThread,
        parent_id: Some(Id(101)),
        name: Some("help-me".into()),
        ..text_channel(104, 1)
    };
    let group = Channel {
        kind: ChannelKind::GroupDm,
        name: Some("trip".into()),
        recipients: vec![user(2), user(3)],
        ..dm(302, 2)
    };
    ready(
        ME,
        vec![g1, guild(2)],
        vec![
            text_channel(101, 1),
            text_channel(102, 1),
            text_channel(103, 1),
            thread,
            text_channel(111, 2),
            dm(301, 2),
            group,
        ],
    )
}

fn at(age_ms: i64) -> Id {
    id_at(NOW - age_ms)
}

/// A message from U002 in a G001 channel.
fn from_u2(id: Id, channel: u64) -> Message {
    Message { guild_id: Some(Id(1)), ..msg(id, channel, 2, "x") }
}

fn mention_me(id: Id, channel: u64) -> Message {
    Message { mentions: vec![Id(ME)], ..from_u2(id, channel) }
}

fn guild_settings(muted: MuteUntil, suppress_everyone: bool) -> crate::discord::types::GuildSettings {
    crate::discord::types::GuildSettings { suppress_everyone, ..settings(Some(1), muted) }
}

fn reasons(items: &[PendingItem]) -> Vec<(u64, Reason)> {
    items.iter().map(|i| (i.channel.0, i.reason)).collect()
}

#[test]
fn dm_unread_is_pending() {
    let mut s = snap();
    unread(&mut s, 301, at(1_000), Some(at(5_000)));
    unread(&mut s, 302, at(2_000), Some(at(2_000)));
    let w = world(s);
    let items = pending(&w.ctx.live, &w.ctx.store, &[], NOW).unwrap();
    assert_eq!(reasons(&items), vec![(301, Reason::Dm)], "the read group DM isn't pending");
    assert_eq!(items[0].label, "@U002 (DM)");
}

#[test]
fn normal_channel_needs_a_mention() {
    let mut s = snap();
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    unread(&mut s, 102, at(1_000), Some(at(5_000)));
    // READY says C102 has a mention (e.g. after a restart, nothing buffered).
    s.read_states.insert(Id(102), read_state(Some(at(5_000)), 2));
    let w = world(s);
    let items = pending(&w.ctx.live, &w.ctx.store, &[], NOW).unwrap();
    assert_eq!(reasons(&items), vec![(102, Reason::Mention)]);
    assert_eq!(items[0].mention_count, 2);
    assert_eq!(items[0].label, "G001 › #C102");
}

#[test]
fn mention_in_discord_muted_server_still_pending() {
    let mut s = snap();
    s.settings.push(guild_settings(MuteUntil::Forever, false));
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    unread(&mut s, 102, at(2_000), Some(at(5_000)));
    unread(&mut s, 103, at(3_000), Some(at(5_000)));
    let w = world(s);
    let reply = Message { reply_to: Some(at(9_000)), reply_to_author: Some(Id(ME)), ..from_u2(at(2_000), 102) };
    // A mention already read doesn't count.
    let old = mention_me(at(6_000), 103);
    let items = pending(&w.ctx.live, &w.ctx.store, &[mention_me(at(1_000), 101), reply, old], NOW).unwrap();
    assert_eq!(reasons(&items), vec![(101, Reason::Mention), (102, Reason::Reply)]);
}

#[test]
fn everyone_suppressed_not_pending() {
    let mut s = snap();
    s.settings.push(guild_settings(MuteUntil::No, true));
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    let w = world(s);
    let everyone = Message { mention_everyone: true, ..from_u2(at(1_000), 101) };
    assert!(pending(&w.ctx.live, &w.ctx.store, &[everyone], NOW).unwrap().is_empty());
}

#[test]
fn everyone_in_discord_muted_server_not_pending() {
    let mut s = snap();
    s.settings.push(guild_settings(MuteUntil::Forever, false));
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    // Discord's badge counted the @everyone and the role ping.
    s.read_states.insert(Id(101), read_state(Some(at(5_000)), 2));
    let w = world(s);
    let everyone = Message { mention_everyone: true, ..from_u2(at(2_000), 101) };
    let role = Message { mention_roles: vec![Id(9)], ..from_u2(at(1_000), 101) };
    assert!(pending(&w.ctx.live, &w.ctx.store, &[everyone, role], NOW).unwrap().is_empty());
}

#[test]
fn role_and_everyone_count_when_not_muted_or_suppressed() {
    let mut s = snap();
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    unread(&mut s, 102, at(1_000), Some(at(5_000)));
    let w = world(s);
    let everyone = Message { mention_everyone: true, ..from_u2(at(1_000), 101) };
    let role = Message { mention_roles: vec![Id(9)], ..from_u2(at(1_000), 102) };
    let items = pending(&w.ctx.live, &w.ctx.store, &[everyone, role], NOW).unwrap();
    assert_eq!(reasons(&items).len(), 2);
    assert!(items.iter().all(|i| i.reason == Reason::Mention));
}

#[test]
fn full_watch_overrides_discord_mute() {
    let mut s = snap();
    s.settings.push(guild_settings(MuteUntil::Forever, false));
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    unread(&mut s, 102, at(1_000), Some(at(5_000)));
    unread(&mut s, 111, at(2_000), Some(at(5_000)));
    let w = world(s);
    w.ctx.store.set_watch(Id(101), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    w.ctx.store.set_watch(Id(2), TargetKind::Guild, Some(WatchLevel::Full)).unwrap();
    let items = pending(&w.ctx.live, &w.ctx.store, &[], NOW).unwrap();
    assert_eq!(reasons(&items), vec![(101, Reason::FullWatch), (111, Reason::FullWatch)]);
}

#[test]
fn effective_watch_order() {
    let mut s = snap();
    s.settings.push(guild_settings(MuteUntil::Forever, false));
    let w = world(s);
    let live = &w.ctx.live;
    let c101 = live.channel(Id(101)).unwrap();
    let thread = live.channel(Id(104)).unwrap();
    let c111 = live.channel(Id(111)).unwrap();
    let dm = live.channel(Id(301)).unwrap();
    let none = WatchMap::default();
    assert_eq!(effective_watch(&c101, live, &none, NOW), WatchLevel::Muted, "Discord mute");
    assert_eq!(effective_watch(&c111, live, &none, NOW), WatchLevel::Normal);
    assert_eq!(effective_watch(&dm, live, &none, NOW), WatchLevel::Full);
    let explicit =
        WatchMap { guilds: [(Id(1), WatchLevel::Full)].into(), channels: [(Id(101), WatchLevel::Normal)].into() };
    assert_eq!(effective_watch(&c101, live, &explicit, NOW), WatchLevel::Normal, "channel beats guild");
    assert_eq!(effective_watch(&thread, live, &explicit, NOW), WatchLevel::Normal, "a thread follows its parent");
    let c102 = live.channel(Id(102)).unwrap();
    assert_eq!(effective_watch(&c102, live, &explicit, NOW), WatchLevel::Full, "guild beats Discord mute");
}

#[test]
fn tritone_muted_hides_even_mentions() {
    let mut s = snap();
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    unread(&mut s, 301, at(1_000), Some(at(5_000)));
    let w = world(s);
    w.ctx.store.set_watch(Id(101), TargetKind::Channel, Some(WatchLevel::Muted)).unwrap();
    w.ctx.store.set_watch(Id(301), TargetKind::Channel, Some(WatchLevel::Muted)).unwrap();
    let dm_ping = Message { guild_id: None, ..mention_me(at(1_000), 301) };
    assert!(pending(&w.ctx.live, &w.ctx.store, &[mention_me(at(1_000), 101), dm_ping], NOW).unwrap().is_empty());
}

#[test]
fn snoozed_hidden_until_time() {
    let mut s = snap();
    unread(&mut s, 301, at(1_000), Some(at(5_000)));
    let w = world(s);
    w.ctx.store.set_snooze(Id(301), Some(NOW + 1_000)).unwrap();
    assert!(pending(&w.ctx.live, &w.ctx.store, &[], NOW).unwrap().is_empty());
    assert_eq!(pending(&w.ctx.live, &w.ctx.store, &[], NOW + 1_000).unwrap().len(), 1);
}

#[test]
fn unread_thread_is_pending() {
    let mut s = snap();
    unread(&mut s, 104, at(1_000), Some(at(5_000)));
    let w = world(s);
    let items = pending(&w.ctx.live, &w.ctx.store, &[], NOW).unwrap();
    assert_eq!(reasons(&items), vec![(104, Reason::Thread)]);
    assert_eq!(items[0].label, "G001 › #C101 › help-me");
}

#[test]
fn pending_order() {
    let mut s = snap();
    unread(&mut s, 301, at(4_000), Some(at(9_000)));
    unread(&mut s, 302, at(3_000), Some(at(9_000)));
    unread(&mut s, 101, at(1_000), Some(at(9_000)));
    unread(&mut s, 102, at(2_000), Some(at(9_000)));
    unread(&mut s, 103, at(500), Some(at(9_000)));
    unread(&mut s, 111, at(100), Some(at(9_000)));
    let w = world(s);
    w.ctx.store.set_watch(Id(103), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    w.ctx.store.set_watch(Id(111), TargetKind::Channel, Some(WatchLevel::Full)).unwrap();
    let mentions = [mention_me(at(2_000), 102), mention_me(at(1_000), 101)];
    let items = pending(&w.ctx.live, &w.ctx.store, &mentions, NOW).unwrap();
    assert_eq!(
        reasons(&items),
        vec![
            (302, Reason::GroupDm),
            (301, Reason::Dm),
            (101, Reason::Mention),
            (102, Reason::Mention),
            (111, Reason::FullWatch),
            (103, Reason::FullWatch),
        ]
    );
    assert_eq!(items[0].label, "trip (group DM)");
}

#[test]
fn nameless_channel_never_pending() {
    let w = world(snap());
    // A message in a channel tritone never saw creates a nameless one.
    let plain = Message { guild_id: Some(Id(1)), ..msg(at(1_000), 150, 2, "x") };
    w.ctx.live.apply(GatewayEvent::MessageCreate(Box::new(plain.clone())), NOW);
    assert!(w.ctx.live.channel(Id(150)).unwrap().name.is_none());
    w.ctx.store.set_watch(Id(1), TargetKind::Guild, Some(WatchLevel::Full)).unwrap();
    let everyone = Message { mention_everyone: true, ..plain };
    assert!(pending(&w.ctx.live, &w.ctx.store, &[everyone], NOW).unwrap().is_empty(), "no direct mention or reply");
}

#[tokio::test]
async fn mention_in_unknown_thread_still_pending() {
    let mut s = snap();
    s.settings.push(guild_settings(MuteUntil::Forever, false));
    let w = world(s);
    // 150: nameless, made by a gateway message. 999, 997, 996, 995: unknown.
    let phantom = Message { guild_id: Some(Id(1)), ..msg(at(9_000), 150, 3, "x") };
    w.ctx.live.apply(GatewayEvent::MessageCreate(Box::new(phantom)), NOW);
    let reply = Message { reply_to: Some(at(9_500)), reply_to_author: Some(Id(ME)), ..from_u2(at(2_000), 150) };
    let everyone = Message { mention_everyone: true, ..from_u2(at(1_500), 997) };
    // Read on another device: the gateway's ack covers it.
    w.ctx.live.apply(GatewayEvent::MessageAck { channel: Id(996), message: at(500), mention_count: None }, NOW);
    let read_elsewhere = mention_me(at(800), 996);
    let in_muted = Message { guild_id: Some(Id(2)), ..mention_me(at(700), 995) };
    w.ctx.store.set_watch(Id(2), TargetKind::Guild, Some(WatchLevel::Muted)).unwrap();
    let inbox = vec![mention_me(at(1_000), 999), reply, everyone, read_elsewhere, in_muted];

    let items = pending(&w.ctx.live, &w.ctx.store, &inbox, NOW).unwrap();
    assert_eq!(reasons(&items), vec![(999, Reason::Mention), (150, Reason::Reply)]);
    assert_eq!(items[0].label, "G001 › (thread or unknown channel)");

    w.fake.mentions.lock().unwrap().extend(inbox);
    let entries = get_pending(&w.ctx).await.unwrap();
    assert_eq!(entries.len(), 2);
    let e = &entries[0];
    assert_eq!(e.messages.len(), 1);
    assert_eq!(e.messages[0].id, at(1_000).to_string());
    assert_eq!(e.messages[0].link, format!("https://discord.com/channels/1/999/{}", at(1_000)));
    assert!(w.fake.calls().iter().all(|c| !matches!(c, Call::History { channel, .. } if *channel == Id(999))));
}

#[test]
fn buffered_direct_mention_counts_without_the_inbox() {
    let mut s = snap();
    s.settings.push(guild_settings(MuteUntil::Forever, false));
    let w = world(s);
    w.ctx.live.set_buffered([Id(101)].into());
    w.ctx.live.apply(GatewayEvent::MessageCreate(Box::new(mention_me(at(1_000), 101))), NOW);
    let items = pending(&w.ctx.live, &w.ctx.store, &[], NOW).unwrap();
    assert_eq!(reasons(&items), vec![(101, Reason::Mention)]);
    assert_eq!(items[0].unread_count_hint, 1);
}

#[tokio::test]
async fn get_pending_does_not_ack() {
    let mut s = snap();
    let acked = at(9_000);
    unread(&mut s, 301, at(1_000), Some(acked));
    let w = world(s);
    let dm_msgs: Vec<Message> = (0..25).map(|i| msg(at(8_000 - i * 100), 301, 2, "x")).collect();
    w.fake.put_history(Id(301), dm_msgs);
    let entries = get_pending(&w.ctx).await.unwrap();
    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.messages.len(), 20, "first 20 unread");
    assert!(e.more_unread);
    assert_eq!(e.messages[0].id, at(8_000).to_string(), "oldest unread first");
    let calls = w.fake.calls();
    assert!(
        calls.contains(&Call::History { channel: Id(301), before: None, after: Some(acked), limit: 21 }),
        "{calls:?}"
    );
    assert!(calls.iter().all(|c| !matches!(c, Call::Ack { .. } | Call::AckBulk(_))), "{calls:?}");
    assert!(w.ctx.live.is_unread(Id(301)));
}

#[tokio::test]
async fn get_pending_uses_the_mentions_inbox() {
    let mut s = snap();
    s.settings.push(guild_settings(MuteUntil::Forever, false));
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    let eight_days = 8 * 86_400_000;
    unread(&mut s, 102, at(eight_days), Some(at(eight_days + 1_000)));
    let w = world(s);
    // One in the last 7 days, one older than the inbox window.
    w.fake.mentions.lock().unwrap().extend([mention_me(at(1_000), 101), mention_me(at(eight_days), 102)]);
    w.fake.put_history(Id(101), vec![mention_me(at(1_000), 101)]);
    let entries = get_pending(&w.ctx).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].item.reason, Reason::Mention);
    assert!(w.fake.calls().contains(&Call::RecentMentions { limit: 100, before: None }));
}

#[tokio::test]
async fn dm_without_read_state_pending_unless_last_is_mine() {
    let mut s = snap();
    // No read-state entry at all, as for a DM never opened.
    unread(&mut s, 301, at(1_000), None);
    unread(&mut s, 302, at(2_000), None);
    let w = world(s);
    w.fake.put_history(Id(301), vec![msg(at(3_000), 301, 2, "x"), msg(at(1_000), 301, ME, "x")]);
    w.fake.put_history(Id(302), vec![msg(at(3_000), 302, ME, "x"), msg(at(2_000), 302, 3, "x")]);
    // Without messages in hand, pending can't tell who wrote last: both are candidates.
    assert_eq!(pending(&w.ctx.live, &w.ctx.store, &[], NOW).unwrap().len(), 2);
    let entries = get_pending(&w.ctx).await.unwrap();
    let ids: Vec<u64> = entries.iter().map(|e| e.item.channel.0).collect();
    assert_eq!(ids, vec![302], "D301's last message is mine: read");
    assert_eq!(entries[0].messages.len(), 2);
}

#[tokio::test]
async fn dropped_dms_do_not_use_the_fetch_budget() {
    let mut s = snap();
    for n in 1..=17u64 {
        s.channels.push(dm(400 + n, 2));
        // No read state; the newest message is mine.
        unread(&mut s, 400 + n, at(n as i64 * 100), None);
    }
    s.settings.push(guild_settings(MuteUntil::Forever, false));
    unread(&mut s, 101, at(5_000), Some(at(9_000)));
    let w = world(s);
    for n in 1..=17u64 {
        w.fake.put_history(Id(400 + n), vec![msg(at(n as i64 * 100), 400 + n, ME, "x")]);
    }
    w.fake.mentions.lock().unwrap().push(mention_me(at(5_000), 101));
    w.fake.put_history(Id(101), vec![mention_me(at(5_000), 101)]);
    let entries = get_pending(&w.ctx).await.unwrap();
    let ids: Vec<u64> = entries.iter().map(|e| e.item.channel.0).collect();
    assert_eq!(ids, vec![101], "every DM with my message last is read");
    assert_eq!(entries[0].messages.len(), 1, "the mention still got its messages");
}

#[tokio::test]
async fn mention_entry_carries_the_ping() {
    let mut s = snap();
    s.settings.push(guild_settings(MuteUntil::Forever, false));
    let month = 30 * 86_400_000;
    unread(&mut s, 101, at(1_000), Some(at(month)));
    let w = world(s);
    let old: Vec<Message> = (1..=30).map(|i| from_u2(at(month - i * 60_000), 101)).collect();
    w.fake.put_history(Id(101), old);
    w.fake.put_history(Id(101), vec![mention_me(at(1_000), 101)]);
    w.fake.mentions.lock().unwrap().push(mention_me(at(1_000), 101));
    let entries = get_pending(&w.ctx).await.unwrap();
    let e = &entries[0];
    assert_eq!(e.item.reason, Reason::Mention);
    assert!(e.more_unread);
    assert!(e.messages.iter().all(|m| m.id != at(1_000).to_string()), "the oldest unread don't reach it");
    let pings: Vec<String> = e.pings.iter().map(|m| m.id.clone()).collect();
    assert_eq!(pings, vec![at(1_000).to_string()]);
}
