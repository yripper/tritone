//! Time and randomness behind traits, so the send queue's pacing can be
//! tested deterministically with a virtual clock.

use std::time::Duration;

use async_trait::async_trait;
use rand::Rng;

#[async_trait]
pub trait Timer: Send + Sync {
    /// Unix milliseconds.
    fn now_ms(&self) -> i64;
    async fn sleep_until(&self, at_ms: i64);

    fn now_secs(&self) -> i64 {
        self.now_ms().div_euclid(1000)
    }
}

/// Unix ms from any timer, for code holding a `&dyn Timer`.
pub fn now_ms(timer: &dyn Timer) -> i64 {
    timer.now_ms()
}

pub trait GapRng: Send + Sync {
    /// Uniform in `min..=max`.
    fn gap_ms(&self, min: u64, max: u64) -> u64;
}

pub struct SystemTimer;

#[async_trait]
impl Timer for SystemTimer {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    async fn sleep_until(&self, at_ms: i64) {
        let wait = at_ms - self.now_ms();
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait as u64)).await;
        }
    }
}

pub struct ThreadRng;

impl GapRng for ThreadRng {
    fn gap_ms(&self, min: u64, max: u64) -> u64 {
        rand::rng().random_range(min..=max)
    }
}

#[cfg(test)]
pub mod fake {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    /// Virtual clock: sleeping just moves time forward.
    pub struct FakeTimer(pub AtomicI64);

    impl FakeTimer {
        pub fn at(ms: i64) -> FakeTimer {
            FakeTimer(AtomicI64::new(ms))
        }

        pub fn advance(&self, ms: i64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Timer for FakeTimer {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }

        async fn sleep_until(&self, at_ms: i64) {
            self.0.fetch_max(at_ms, Ordering::SeqCst);
        }
    }

    /// Hands out the given gaps in order, then repeats the last one.
    pub struct SeqGaps(pub Mutex<VecDeque<u64>>);

    impl SeqGaps {
        pub fn new(gaps: &[u64]) -> SeqGaps {
            SeqGaps(Mutex::new(gaps.iter().copied().collect()))
        }
    }

    impl GapRng for SeqGaps {
        fn gap_ms(&self, min: u64, max: u64) -> u64 {
            let mut q = self.0.lock().unwrap();
            let g = if q.len() > 1 { q.pop_front().unwrap_or(min) } else { q.front().copied().unwrap_or(min) };
            g.clamp(min, max)
        }
    }
}

#[cfg(test)]
#[path = "clock_tests.rs"]
mod tests;
