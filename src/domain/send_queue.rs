//! Paces every write: one at a time, typing first, random gaps, hourly cap.
//! Stub with the final public signatures.

use std::sync::Arc;

use crate::discord::DiscordApi;
use crate::domain::clock::{GapRng, Timer};
use crate::domain::errors::{Error, Result};
use crate::store::Store;

pub struct JobSpec;
pub struct Enqueued;
pub struct QueueStatus;

pub struct SendQueue {
    _store: Arc<Store>,
    _timer: Arc<dyn Timer>,
    _rng: Arc<dyn GapRng>,
}

impl SendQueue {
    pub fn new(store: Arc<Store>, timer: Arc<dyn Timer>, rng: Arc<dyn GapRng>) -> SendQueue {
        SendQueue { _store: store, _timer: timer, _rng: rng }
    }

    pub fn attach(&self, api: Arc<dyn DiscordApi>) {
        drop(api);
    }

    pub async fn run(self: Arc<Self>) {}

    pub fn enqueue(&self, job: JobSpec) -> Result<Enqueued> {
        let _ = job;
        Err(Error::Invalid("not implemented".into()))
    }

    pub fn status(&self, job_id: Option<u64>) -> Result<QueueStatus> {
        let _ = job_id;
        Err(Error::Invalid("not implemented".into()))
    }
}
