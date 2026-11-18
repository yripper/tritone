//! A `FakeDiscord` wrapper for domain tests: stamps every call with the
//! virtual time and fails chosen methods once.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;

use crate::discord::api::{DResult, DiscordApi};
use crate::discord::fake::{Call, FakeDiscord};
use crate::discord::types::{
    Channel, DiscordError, Id, Message, MuteTarget, OutgoingMessage, SearchPage, SearchQuery, SearchScope, UserRef,
};
use crate::domain::clock::Timer;
use crate::domain::clock::fake::FakeTimer;

/// Unix ms the tests start at.
pub const T0: i64 = 1_800_000_000_000;

/// `FakeDiscord` plus the virtual time of every call and per-method failures.
pub struct Api {
    pub inner: FakeDiscord,
    pub timer: Arc<FakeTimer>,
    times: StdMutex<Vec<(Call, i64)>>,
    /// (method, nth, error): the nth next call of that method fails, once.
    fail: StdMutex<Vec<(&'static str, u32, DiscordError)>>,
}

impl Api {
    pub fn new(timer: Arc<FakeTimer>) -> Api {
        Api { inner: FakeDiscord::default(), timer, times: StdMutex::new(Vec::new()), fail: StdMutex::new(Vec::new()) }
    }

    pub fn calls(&self) -> Vec<Call> {
        self.inner.calls()
    }

    pub fn fail_on(&self, method: &'static str, e: DiscordError) {
        self.fail_on_nth(method, 1, e);
    }

    /// The `nth` next call of `method` (1 = the next one) fails once.
    pub fn fail_on_nth(&self, method: &'static str, nth: u32, e: DiscordError) {
        self.fail.lock().unwrap().push((method, nth, e));
    }

    /// When each call of the given kind happened, relative to T0.
    pub fn times_of(&self, pick: fn(&Call) -> bool) -> Vec<i64> {
        self.times.lock().unwrap().iter().filter(|(c, _)| pick(c)).map(|(_, t)| t - T0).collect()
    }

    fn check(&self, method: &'static str) -> DResult<()> {
        let mut f = self.fail.lock().unwrap();
        let Some(i) = f.iter().position(|(m, _, _)| *m == method) else { return Ok(()) };
        if f[i].1 > 1 {
            f[i].1 -= 1;
            return Ok(());
        }
        Err(f.remove(i).2)
    }

    fn stamp(&self) {
        let last = self.inner.calls().last().cloned();
        if let Some(c) = last {
            self.times.lock().unwrap().push((c, self.timer.now_ms()));
        }
    }
}

macro_rules! fwd {
    ($self:ident, $m:literal, $call:expr) => {{
        let r = $call.await;
        $self.stamp();
        $self.check($m)?;
        r
    }};
}

#[async_trait]
impl DiscordApi for Api {
    async fn history(&self, c: Id, b: Option<Id>, a: Option<Id>, l: u8) -> DResult<Vec<Message>> {
        fwd!(self, "history", self.inner.history(c, b, a, l))
    }
    async fn around(&self, c: Id, m: Id, l: u8) -> DResult<Vec<Message>> {
        fwd!(self, "around", self.inner.around(c, m, l))
    }
    async fn recent_mentions(&self, l: u8, b: Option<Id>) -> DResult<Vec<Message>> {
        fwd!(self, "recent_mentions", self.inner.recent_mentions(l, b))
    }
    async fn search(&self, s: SearchScope, q: &SearchQuery) -> DResult<SearchPage> {
        fwd!(self, "search", self.inner.search(s, q))
    }
    async fn ack(&self, c: Id, m: Id) -> DResult<()> {
        fwd!(self, "ack", self.inner.ack(c, m))
    }
    async fn ack_bulk(&self, i: &[(Id, Id)]) -> DResult<()> {
        fwd!(self, "ack_bulk", self.inner.ack_bulk(i))
    }
    async fn typing(&self, c: Id) -> DResult<()> {
        fwd!(self, "typing", self.inner.typing(c))
    }
    async fn send(&self, c: Id, m: &OutgoingMessage) -> DResult<Message> {
        // A failed send must not land in the fake's history: record it by hand.
        if let Err(e) = self.check("send") {
            self.inner.calls.lock().unwrap().push(Call::Send { channel: c, msg: m.clone() });
            self.stamp();
            return Err(e);
        }
        let r = self.inner.send(c, m).await;
        self.stamp();
        r
    }
    async fn react(&self, c: Id, m: Id, e: &str) -> DResult<()> {
        fwd!(self, "react", self.inner.react(c, m, e))
    }
    async fn create_thread(&self, c: Id, f: Option<Id>, n: &str) -> DResult<Channel> {
        fwd!(self, "create_thread", self.inner.create_thread(c, f, n))
    }
    async fn create_forum_post(&self, f: Id, n: &str, c: &str, nonce: Id) -> DResult<(Channel, Message)> {
        fwd!(self, "create_forum_post", self.inner.create_forum_post(f, n, c, nonce))
    }
    async fn leave_guild(&self, g: Id) -> DResult<()> {
        fwd!(self, "leave_guild", self.inner.leave_guild(g))
    }
    async fn set_mute(&self, t: MuteTarget, m: bool) -> DResult<()> {
        fwd!(self, "set_mute", self.inner.set_mute(t, m))
    }
    async fn user(&self, u: Id) -> DResult<UserRef> {
        fwd!(self, "user", self.inner.user(u))
    }
}
