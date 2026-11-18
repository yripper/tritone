//! A connected `Ctx` over `FakeDiscord`, a `LiveState` loaded from a READY
//! fixture and an in-memory store, for the triage tests.

use std::sync::Arc;

use crate::config::Config;
use crate::discord::fake::FakeDiscord;
use crate::discord::gateway::GatewayEvent;
use crate::discord::types::{Channel, Id, ReadySnapshot};
use crate::domain::clock::fake::{FakeTimer, SeqGaps};
use crate::domain::live::{BufferPolicy, LiveState};
use crate::domain::send_queue::SendQueue;
use crate::mcp::state::Ctx;
use crate::store::Store;
use crate::test_support::{read_state, user};

pub const ME: u64 = 7;
pub const NOW: i64 = 1_800_000_000_000;

pub struct World {
    pub ctx: Ctx,
    pub fake: Arc<FakeDiscord>,
}

pub fn world(snap: ReadySnapshot) -> World {
    let store = Arc::new(Store::open_in_memory().unwrap());
    let timer = Arc::new(FakeTimer::at(NOW));
    let live = Arc::new(LiveState::new(BufferPolicy { per_channel: 50, max_age_ms: 86_400_000 }));
    live.apply(GatewayEvent::Ready(Box::new(snap)), NOW);
    let config = Arc::new(Config::default());
    let queue = Arc::new(SendQueue::new(
        store.clone(),
        live.clone(),
        config.clone(),
        timer.clone(),
        Arc::new(SeqGaps::new(&[0])),
    ));
    let fake = Arc::new(FakeDiscord::new(user(ME)));
    let ctx = Ctx { store, live, api: fake.clone(), queue, config, timer };
    World { ctx, fake }
}

/// Gives a channel in the snapshot a last message and, optionally, a read state.
pub fn unread(snap: &mut ReadySnapshot, channel: u64, last: Id, acked: Option<Id>) {
    let c: &mut Channel = snap.channels.iter_mut().find(|c| c.id == Id(channel)).expect("channel in snapshot");
    c.last_message_id = Some(last);
    if let Some(a) = acked {
        snap.read_states.insert(Id(channel), read_state(Some(a), 0));
    }
}
