use super::*;
use crate::test_support::msg;

fn fake_with_history() -> FakeDiscord {
    let f = FakeDiscord::default();
    f.put_history(Id(101), (1..=10).map(|i| msg(Id(i), 101, 2, &format!("m{i}"))).collect());
    f
}

fn ids(v: &[Message]) -> Vec<u64> {
    v.iter().map(|m| m.id.0).collect()
}

#[tokio::test]
async fn history_is_newest_first_and_pages() {
    let f = fake_with_history();
    assert_eq!(ids(&f.history(Id(101), None, None, 3).await.unwrap()), [10, 9, 8]);
    assert_eq!(ids(&f.history(Id(101), Some(Id(8)), None, 3).await.unwrap()), [7, 6, 5]);
    assert_eq!(ids(&f.history(Id(101), None, Some(Id(2)), 3).await.unwrap()), [5, 4, 3]);
    assert!(f.history(Id(102), None, None, 3).await.unwrap().is_empty());
    assert_eq!(f.calls()[0], Call::History { channel: Id(101), before: None, after: None, limit: 3 });
}

#[tokio::test]
async fn around_centers_on_the_message() {
    let f = fake_with_history();
    assert_eq!(ids(&f.around(Id(101), Id(5), 4).await.unwrap()), [7, 6, 5, 4]);
}

#[tokio::test]
async fn send_returns_message_with_nonce_id_and_records_it() {
    let f = FakeDiscord::default();
    let out = OutgoingMessage { content: "hi".into(), reply_to: Some(Id(3)), nonce: Id(77), silent: false };
    let m = f.send(Id(101), &out).await.unwrap();
    assert_eq!(m.id, Id(77));
    assert_eq!(m.author.id, f.me.id);
    assert_eq!(m.reply_to, Some(Id(3)));
    assert_eq!(f.sent.lock().unwrap().len(), 1);
    assert_eq!(ids(&f.history(Id(101), None, None, 5).await.unwrap()), [77]);
}

#[tokio::test]
async fn next_error_is_returned_once() {
    let f = FakeDiscord::default();
    f.fail_next(DiscordError::Captcha);
    assert_eq!(f.typing(Id(101)).await, Err(DiscordError::Captcha));
    assert_eq!(f.typing(Id(101)).await, Ok(()));
    assert_eq!(f.calls().len(), 2, "the failed call is still recorded");
}

#[tokio::test]
async fn search_filters_and_pages() {
    let f = FakeDiscord::default();
    let mut all = Vec::new();
    for i in 1..=30 {
        let mut m = msg(Id(i), 101, if i % 2 == 0 { 1 } else { 2 }, "x");
        m.guild_id = Some(Id(9));
        all.push(m);
    }
    *f.search_index.lock().unwrap() = all;
    let q = SearchQuery { author_id: Some(Id(1)), ..Default::default() };
    let page = f.search(SearchScope::Guild(Id(9)), &q).await.unwrap();
    assert_eq!(page.total, 15);
    assert_eq!(page.messages[0].id, Id(30));
    let page = f.search(SearchScope::Guild(Id(9)), &SearchQuery { offset: 25, ..Default::default() }).await.unwrap();
    assert_eq!(page.total, 30);
    assert_eq!(page.messages.len(), 5);
    assert_eq!(f.search(SearchScope::Guild(Id(8)), &q).await.unwrap().total, 0);
}

#[tokio::test]
async fn user_lookup_and_writes_are_recorded() {
    let f = FakeDiscord::default();
    assert_eq!(f.user(Id(5)).await, Err(DiscordError::NotFound));
    f.users.lock().unwrap().insert(Id(5), crate::test_support::user(5));
    assert_eq!(f.user(Id(5)).await.unwrap().username, "U005");
    f.react(Id(101), Id(1), "👍").await.unwrap();
    f.set_mute(MuteTarget::Guild(Id(9)), true).await.unwrap();
    f.leave_guild(Id(9)).await.unwrap();
    f.ack_bulk(&[(Id(101), Id(1))]).await.unwrap();
    let t = f.create_thread(Id(101), Some(Id(1)), "t").await.unwrap();
    assert_eq!((t.id, t.parent_id), (Id(1), Some(Id(101))));
    let (post, first) = f.create_forum_post(Id(300), "q", "body", Id(88)).await.unwrap();
    assert_eq!((post.id, first.id, post.parent_id), (Id(88), Id(88), Some(Id(300))));
    assert!(f.calls().contains(&Call::LeaveGuild(Id(9))));
}
