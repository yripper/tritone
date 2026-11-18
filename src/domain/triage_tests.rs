use super::*;
use crate::discord::fake::Call;
use crate::discord::types::{Channel, Id};
use crate::domain::errors::Error;
use crate::domain::test_world::{ME, NOW, unread, world};
use crate::store::{TargetKind, WatchLevel};
use crate::test_support::*;

fn at(age_ms: i64) -> Id {
    id_at(NOW - age_ms)
}

#[tokio::test]
async fn mark_read_chunks_by_100() {
    let channels: Vec<Channel> = (1..=250).map(|n| text_channel(1_000 + n, 1)).collect();
    let mut s = ready(ME, vec![guild(1)], channels);
    for n in 1..=250 {
        unread(&mut s, 1_000 + n, at(n as i64), Some(at(10_000)));
    }
    let w = world(s);
    let ids: Vec<Id> = (1..=250).map(|n| Id(1_000 + n)).collect();
    let r = mark_read(&w.ctx, &ids).await.unwrap();
    assert_eq!(r.marked.len(), 250);
    let sizes: Vec<usize> = w
        .fake
        .calls()
        .iter()
        .filter_map(|c| match c {
            Call::AckBulk(items) => Some(items.len()),
            _ => None,
        })
        .collect();
    assert_eq!(sizes, vec![100, 100, 50]);
    let first: Vec<(Id, Id)> = (1..=100).map(|n| (Id(1_000 + n), at(n as i64))).collect();
    assert_eq!(w.fake.calls()[0], Call::AckBulk(first), "each channel acked to its last message, in order");
    assert!(ids.iter().all(|c| !w.ctx.live.is_unread(*c)));
}

#[tokio::test]
async fn mark_read_skips_channels_with_nothing_unread() {
    let mut s = ready(ME, vec![guild(1)], vec![text_channel(101, 1), text_channel(102, 1), text_channel(103, 1)]);
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    unread(&mut s, 102, at(1_000), Some(at(1_000)));
    let w = world(s);
    let r = mark_read(&w.ctx, &[Id(101), Id(102), Id(103)]).await.unwrap();
    assert_eq!(r.marked, vec![Id(101)]);
    assert_eq!(r.already_read, vec![Id(102), Id(103)]);
    assert_eq!(w.fake.calls(), vec![Call::AckBulk(vec![(Id(101), at(1_000))])]);
    let none = mark_read(&w.ctx, &[Id(102)]).await.unwrap();
    assert!(none.marked.is_empty());
    assert_eq!(w.fake.calls().len(), 1, "nothing to ack: no call");
}

#[tokio::test]
async fn mark_read_failure_leaves_channels_unread() {
    let mut s = ready(ME, vec![guild(1)], vec![text_channel(101, 1)]);
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    let w = world(s);
    w.fake.fail_next(crate::discord::types::DiscordError::Captcha);
    let e = mark_read(&w.ctx, &[Id(101)]).await.unwrap_err();
    assert!(matches!(e, Error::Discord(crate::discord::types::DiscordError::Captcha)));
    assert!(w.ctx.live.is_unread(Id(101)));
}

#[test]
fn snooze_and_unsnooze() {
    let w = world(ready(ME, vec![guild(1)], vec![text_channel(101, 1)]));
    assert!(matches!(snooze(&w.ctx, Id(101), NOW - 1), Err(Error::Invalid(_))), "past times are refused");
    snooze(&w.ctx, Id(101), NOW + 60_000).unwrap();
    assert_eq!(w.ctx.store.snoozed_until(Id(101)).unwrap(), Some(NOW + 60_000));
    unsnooze(&w.ctx, Id(101)).unwrap();
    assert_eq!(w.ctx.store.snoozed_until(Id(101)).unwrap(), None);
}

#[test]
fn set_watch_infers_guild_or_channel() {
    let w = world(ready(ME, vec![guild(1)], vec![text_channel(101, 1), dm(301, 2)]));
    assert_eq!(set_watch(&w.ctx, Id(1), None, Some(WatchLevel::Full)).unwrap(), TargetKind::Guild);
    assert_eq!(set_watch(&w.ctx, Id(101), None, Some(WatchLevel::Muted)).unwrap(), TargetKind::Channel);
    assert_eq!(set_watch(&w.ctx, Id(301), None, Some(WatchLevel::Normal)).unwrap(), TargetKind::Channel);
    let all = w.ctx.store.all_watch().unwrap();
    assert_eq!(all.guilds.get(&Id(1)), Some(&WatchLevel::Full));
    assert_eq!(all.channels.get(&Id(101)), Some(&WatchLevel::Muted));
    set_watch(&w.ctx, Id(101), None, None).unwrap();
    assert_eq!(w.ctx.store.get_watch(Id(101), TargetKind::Channel).unwrap(), None, "clear");
    assert!(matches!(set_watch(&w.ctx, Id(999), None, Some(WatchLevel::Full)), Err(Error::NotFound(_))));
    let wrong = set_watch(&w.ctx, Id(101), Some(TargetKind::Guild), Some(WatchLevel::Full));
    assert!(matches!(wrong, Err(Error::NotFound(_))), "101 is no server");
}

#[test]
fn guild_and_default_channel_same_id_both_watchable() {
    // Older servers' default channel has the server's id.
    let general = Channel { name: Some("general".into()), ..text_channel(1, 1) };
    let w = world(ready(ME, vec![guild(1)], vec![general, text_channel(102, 1)]));
    let e = set_watch(&w.ctx, Id(1), None, Some(WatchLevel::Muted)).unwrap_err();
    assert!(matches!(e, Error::Invalid(ref m) if m.contains("server name")), "a bare shared id must say which: {e}");
    set_watch(&w.ctx, Id(1), Some(TargetKind::Channel), Some(WatchLevel::Muted)).unwrap();
    set_watch(&w.ctx, Id(1), Some(TargetKind::Guild), Some(WatchLevel::Full)).unwrap();
    let all = w.ctx.store.all_watch().unwrap();
    let (live, now) = (&w.ctx.live, crate::domain::test_world::NOW);
    let effective = |c: u64| crate::domain::pending::effective_watch(&live.channel(Id(c)).unwrap(), live, &all, now);
    assert_eq!(effective(1), WatchLevel::Muted, "#general muted");
    assert_eq!(effective(102), WatchLevel::Full, "the rest of the server keeps the server's level");
}

#[tokio::test]
async fn mark_read_dedupes_and_reports_unknown_ids() {
    let mut s = ready(ME, vec![guild(1)], vec![text_channel(101, 1)]);
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    let w = world(s);
    let r = mark_read(&w.ctx, &[Id(101), Id(4242), Id(101)]).await.unwrap();
    assert_eq!(r.marked, vec![Id(101)]);
    assert_eq!(r.not_found, vec![Id(4242)]);
    assert!(r.already_read.is_empty());
    assert_eq!(w.fake.calls(), vec![Call::AckBulk(vec![(Id(101), at(1_000))])]);
}
