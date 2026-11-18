use super::*;
use crate::discord::fake::Call;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{
    Channel, ChannelKind, Id, Message, MuteUntil, ReadySnapshot, SearchQuery, SearchScope, UserRef,
};
use crate::domain::test_world::{ME, NOW, unread, world};
use crate::store::{TargetKind, WatchLevel};
use crate::test_support::*;

/// G001 with C101, C102; G002 "Rust Lang" with C111; DM D301 with U002 ("Sam").
fn snap() -> ReadySnapshot {
    let mut g2 = guild(2);
    g2.name = "Rust Lang".into();
    let mut d = dm(301, 2);
    d.recipients = vec![UserRef { display_name: Some("Sam".into()), ..user(2) }];
    let mut s =
        ready(ME, vec![guild(1), g2], vec![text_channel(101, 1), text_channel(102, 1), text_channel(111, 2), d]);
    s.users = vec![user(3), UserRef { display_name: Some("Sammy".into()), ..user(4) }];
    s
}

fn at(age_ms: i64) -> Id {
    id_at(NOW - age_ms)
}

fn three_messages(w: &crate::domain::test_world::World) -> Vec<Id> {
    let ids = vec![at(3_000), at(2_000), at(1_000)];
    let msgs = ids.iter().map(|id| Message { guild_id: Some(Id(1)), ..msg(*id, 101, 2, "x") }).collect();
    w.fake.put_history(Id(101), msgs);
    ids
}

fn acks(calls: &[Call]) -> usize {
    calls.iter().filter(|c| matches!(c, Call::Ack { .. } | Call::AckBulk(_))).count()
}

#[tokio::test]
async fn read_history_acks_newest() {
    let mut s = snap();
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    let w = world(s);
    let ids = three_messages(&w);
    let h = read_history(&w.ctx, Id(101), 50, None, false).await.unwrap();
    assert_eq!(h.channel_label, "G001 › #C101");
    let got: Vec<String> = h.messages.iter().map(|m| m.id.clone()).collect();
    assert_eq!(got, ids.iter().map(Id::to_string).collect::<Vec<_>>(), "oldest first");
    assert_eq!(h.newest_id, Some(at(1_000).to_string()));
    assert!(!h.more);
    assert!(w.fake.calls().contains(&Call::Ack { channel: Id(101), message: at(1_000) }));
    assert_eq!(w.ctx.live.read_state(Id(101)).last_acked, Some(at(1_000)));
    assert!(!w.ctx.live.is_unread(Id(101)));
}

#[tokio::test]
async fn read_history_when_already_read_sends_no_ack() {
    let mut s = snap();
    unread(&mut s, 101, at(1_000), Some(at(1_000)));
    let w = world(s);
    three_messages(&w);
    read_history(&w.ctx, Id(101), 50, None, false).await.unwrap();
    assert_eq!(acks(&w.fake.calls()), 0);
}

#[tokio::test]
async fn peek_never_acks() {
    let mut s = snap();
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    let w = world(s);
    three_messages(&w);
    let h = read_history(&w.ctx, Id(101), 2, None, true).await.unwrap();
    assert_eq!(h.messages.len(), 2);
    assert!(h.more, "a full page means there may be more");
    assert_eq!(acks(&w.fake.calls()), 0);
    assert!(w.ctx.live.is_unread(Id(101)));
}

#[tokio::test]
async fn read_with_before_does_not_ack() {
    let mut s = snap();
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    let w = world(s);
    three_messages(&w);
    let h = read_history(&w.ctx, Id(101), 50, Some(at(1_000)), false).await.unwrap();
    assert_eq!(h.messages.len(), 2);
    assert!(w.fake.calls().contains(&Call::History {
        channel: Id(101),
        before: Some(at(1_000)),
        after: None,
        limit: 50
    }));
    assert_eq!(acks(&w.fake.calls()), 0);
    assert!(w.ctx.live.is_unread(Id(101)));
}

#[tokio::test]
async fn failed_ack_still_returns_the_messages() {
    let mut s = snap();
    unread(&mut s, 101, at(1_000), Some(at(5_000)));
    let w = world(s);
    three_messages(&w);
    // The history call takes the first programmed error, so fail after it.
    let ctx = w.ctx.clone();
    let fake = w.fake.clone();
    let h = {
        let api = FailAck(fake.clone());
        let ctx = crate::mcp::state::Ctx { api: std::sync::Arc::new(api), ..ctx };
        read_history(&ctx, Id(101), 50, None, false).await.unwrap()
    };
    assert_eq!(h.messages.len(), 3);
    assert!(h.ack_error.as_deref().is_some_and(|e| e.contains("rate limited")), "{:?}", h.ack_error);
    assert!(w.ctx.live.is_unread(Id(101)));
}

/// Delegates to the fake but fails every ack.
struct FailAck(std::sync::Arc<crate::discord::fake::FakeDiscord>);

#[async_trait::async_trait]
impl crate::discord::DiscordApi for FailAck {
    async fn history(&self, c: Id, b: Option<Id>, a: Option<Id>, l: u8) -> crate::discord::api::DResult<Vec<Message>> {
        self.0.history(c, b, a, l).await
    }
    async fn around(&self, c: Id, m: Id, l: u8) -> crate::discord::api::DResult<Vec<Message>> {
        self.0.around(c, m, l).await
    }
    async fn recent_mentions(&self, l: u8, b: Option<Id>) -> crate::discord::api::DResult<Vec<Message>> {
        self.0.recent_mentions(l, b).await
    }
    async fn search(
        &self,
        s: crate::discord::types::SearchScope,
        q: &crate::discord::types::SearchQuery,
    ) -> crate::discord::api::DResult<crate::discord::types::SearchPage> {
        self.0.search(s, q).await
    }
    async fn ack(&self, _: Id, _: Id) -> crate::discord::api::DResult<()> {
        Err(crate::discord::types::DiscordError::RateLimited { retry_after_s: 5 })
    }
    async fn ack_bulk(&self, _: &[(Id, Id)]) -> crate::discord::api::DResult<()> {
        Err(crate::discord::types::DiscordError::RateLimited { retry_after_s: 5 })
    }
    async fn typing(&self, c: Id) -> crate::discord::api::DResult<()> {
        self.0.typing(c).await
    }
    async fn send(&self, c: Id, m: &crate::discord::types::OutgoingMessage) -> crate::discord::api::DResult<Message> {
        self.0.send(c, m).await
    }
    async fn react(&self, c: Id, m: Id, e: &str) -> crate::discord::api::DResult<()> {
        self.0.react(c, m, e).await
    }
    async fn create_thread(&self, c: Id, m: Option<Id>, n: &str) -> crate::discord::api::DResult<Channel> {
        self.0.create_thread(c, m, n).await
    }
    async fn create_forum_post(
        &self,
        f: Id,
        n: &str,
        c: &str,
        nonce: Id,
    ) -> crate::discord::api::DResult<(Channel, Message)> {
        self.0.create_forum_post(f, n, c, nonce).await
    }
    async fn leave_guild(&self, g: Id) -> crate::discord::api::DResult<()> {
        self.0.leave_guild(g).await
    }
    async fn set_mute(&self, t: crate::discord::types::MuteTarget, m: bool) -> crate::discord::api::DResult<()> {
        self.0.set_mute(t, m).await
    }
    async fn user(&self, u: Id) -> crate::discord::api::DResult<UserRef> {
        self.0.user(u).await
    }
}

#[test]
fn listings_hide_nameless_and_muted_channels() {
    let mut s = snap();
    s.settings.push(settings(Some(2), MuteUntil::Forever));
    unread(&mut s, 102, at(1_000), Some(at(5_000)));
    let w = world(s);
    let phantom = Message { guild_id: Some(Id(1)), ..msg(at(1_000), 150, 2, "x") };
    w.ctx.live.apply(GatewayEvent::MessageCreate(Box::new(phantom)), NOW);
    w.ctx.store.set_watch(Id(101), TargetKind::Channel, Some(WatchLevel::Muted)).unwrap();
    let (live, store) = (&w.ctx.live, &w.ctx.store);

    let all = list_channels(live, store, None, None, None, NOW).unwrap();
    let ids: Vec<u64> = all.iter().map(|c| c.id.0).collect();
    assert_eq!(ids, vec![301, 102], "DMs first, then by label; muted (tritone or Discord) and nameless hidden");
    let c102 = &all[1];
    assert!(c102.unread);
    assert_eq!((c102.kind, c102.watch, c102.label.as_str()), ("text", WatchLevel::Normal, "G001 › #C102"));

    let muted = list_channels(live, store, None, None, Some(WatchLevel::Muted), NOW).unwrap();
    let ids: Vec<u64> = muted.iter().map(|c| c.id.0).collect();
    assert_eq!(ids, vec![101, 111], "asking for muted shows them");
    let dms = list_channels(live, store, Some(KindFilter::Dm), None, None, NOW).unwrap();
    assert_eq!(dms.len(), 1);
    assert_eq!(dms[0].label, "@Sam (DM)");
    let g1 = list_channels(live, store, None, Some(Id(1)), None, NOW).unwrap();
    assert_eq!(g1.len(), 1);

    let servers = list_servers(live, store, NOW, false).unwrap();
    assert_eq!(servers.len(), 2, "Discord-muted servers are listed, flagged");
    let rust = servers.iter().find(|s| s.id == Id(2)).unwrap();
    assert!(rust.discord_muted);
    let g001 = servers.iter().find(|s| s.id == Id(1)).unwrap();
    assert_eq!(g001.unread_channels, 1, "the nameless channel doesn't count");
    store.set_watch(Id(2), TargetKind::Guild, Some(WatchLevel::Muted)).unwrap();
    assert_eq!(list_servers(live, store, NOW, false).unwrap().len(), 1);
    assert_eq!(list_servers(live, store, NOW, true).unwrap().len(), 2);
}

#[test]
fn find_user_by_name_or_display_name() {
    let w = world(snap());
    let m = find_user(&w.ctx.live, "sam");
    let ids: Vec<u64> = m.iter().map(|u| u.id.0).collect();
    assert_eq!(ids, vec![2, 4]);
    assert_eq!(m[0].dm_channel, Some(Id(301)));
    assert_eq!(m[0].label, "Sam");
    assert!(find_user(&w.ctx.live, "nobody").is_empty());
    let kind = w.ctx.live.channel(Id(301)).unwrap().kind;
    assert_eq!(kind, ChannelKind::Dm);
}

#[test]
fn find_user_lists_servers_seen_in() {
    let w = world(snap());
    w.ctx.live.set_buffered([Id(111)].into());
    let m = Message { guild_id: Some(Id(2)), ..msg(at(1_000), 111, 3, "x") };
    w.ctx.live.apply(GatewayEvent::MessageCreate(Box::new(m)), NOW);
    let found = find_user(&w.ctx.live, "U003");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].seen_in_servers, vec!["Rust Lang".to_string()]);
}

#[tokio::test]
async fn rest_reads_record_guild_authors() {
    let w = world(snap());
    let asker = UserRef { display_name: Some("Kim".into()), ..user(5) };
    w.fake.put_history(Id(101), vec![Message { author: asker.clone(), ..msg(at(1_000), 101, 5, "q") }]);
    read_history(&w.ctx, Id(101), 50, None, true).await.unwrap();
    assert_eq!(w.ctx.live.guild_people(Id(1)), vec![asker]);

    let found = Message { guild_id: Some(Id(2)), ..msg(at(2_000), 111, 6, "hit") };
    w.fake.search_index.lock().unwrap().push(found);
    search(&w.ctx, SearchScope::Guild(Id(2)), &SearchQuery::default()).await.unwrap();
    assert_eq!(w.ctx.live.guild_people(Id(2)).iter().map(|u| u.id).collect::<Vec<_>>(), vec![Id(6)]);
}
