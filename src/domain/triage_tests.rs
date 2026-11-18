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
    assert_eq!(set_watch(&w.ctx, Id(1), Some(WatchLevel::Full)).unwrap(), TargetKind::Guild);
    assert_eq!(set_watch(&w.ctx, Id(101), Some(WatchLevel::Muted)).unwrap(), TargetKind::Channel);
    assert_eq!(set_watch(&w.ctx, Id(301), Some(WatchLevel::Normal)).unwrap(), TargetKind::Channel);
    let all = w.ctx.store.all_watch().unwrap();
    assert_eq!(all.get(&Id(1)), Some(&WatchLevel::Full));
    assert_eq!(all.get(&Id(101)), Some(&WatchLevel::Muted));
    set_watch(&w.ctx, Id(101), None).unwrap();
    assert_eq!(w.ctx.store.get_watch(Id(101)).unwrap(), None, "clear");
    assert!(matches!(set_watch(&w.ctx, Id(999), Some(WatchLevel::Full)), Err(Error::NotFound(_))));
}

#[test]
fn set_watch_refuses_an_id_shared_by_server_and_channel() {
    // Older servers' default channel has the server's id.
    let general = Channel { name: Some("general".into()), ..text_channel(1, 1) };
    let w = world(ready(ME, vec![guild(1)], vec![general, text_channel(102, 1)]));
    let e = set_watch(&w.ctx, Id(1), Some(WatchLevel::Muted)).unwrap_err();
    assert!(matches!(e, Error::Invalid(ref m) if m.contains("same id")), "{e}");
    assert!(w.ctx.store.all_watch().unwrap().is_empty(), "nothing stored");
}
