//! REST pacing: one request in flight at a time, a minimum gap between
//! requests (longer after a write), and a client-wide cooldown after a 429.
//!
//! Discord bans an IP for 24 h after 10,000 401/403/429 answers in 10 minutes
//! (https://docs.discord.food/topics/rate-limits#invalid-request-limit), so a
//! 429 stops every call until its `retry_after` has passed instead of letting
//! each caller find out on its own.

use std::time::Duration;

use tokio::sync::{Mutex, MutexGuard};
use tokio::time::Instant;

/// Gap after any request.
pub const MIN_GAP: Duration = Duration::from_millis(250);
/// Gap between two writes (POST, PUT, PATCH, DELETE).
pub const WRITE_GAP: Duration = Duration::from_millis(500);

#[derive(Default)]
struct Gaps {
    last: Option<Instant>,
    last_write: Option<Instant>,
    cooldown_until: Option<Instant>,
}

pub struct Pacer {
    gaps: Mutex<Gaps>,
}

/// The right to make one request. Holding it keeps every other request
/// waiting; dropping it starts the next gap.
pub struct Turn<'a> {
    gaps: MutexGuard<'a, Gaps>,
    write: bool,
}

impl Default for Pacer {
    fn default() -> Pacer {
        Pacer::new()
    }
}

impl Pacer {
    pub fn new() -> Pacer {
        Pacer { gaps: Mutex::new(Gaps::default()) }
    }

    /// Waits for this request's turn. `Err(seconds left)` while a 429
    /// cooldown runs: the caller answers `RateLimited` without a request.
    pub async fn turn(&self, write: bool) -> Result<Turn<'_>, u64> {
        let gaps = self.gaps.lock().await;
        let now = Instant::now();
        if let Some(until) = gaps.cooldown_until.filter(|u| *u > now) {
            return Err(ceil_secs(until - now));
        }
        let mut ready = gaps.last.map(|t| t + MIN_GAP);
        if write && let Some(w) = gaps.last_write.map(|t| t + WRITE_GAP) {
            ready = Some(ready.map_or(w, |r| r.max(w)));
        }
        if let Some(at) = ready.filter(|at| *at > now) {
            tokio::time::sleep_until(at).await;
        }
        Ok(Turn { gaps, write })
    }
}

impl Turn<'_> {
    /// Refuse every call for `wait` (a 429's `retry_after`).
    pub fn cool_down(&mut self, wait: Duration) {
        self.gaps.cooldown_until = Some(Instant::now() + wait);
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        let now = Instant::now();
        self.gaps.last = Some(now);
        if self.write {
            self.gaps.last_write = Some(now);
        }
    }
}

/// Whole seconds, rounded up, at least 1.
pub fn ceil_secs(d: Duration) -> u64 {
    d.as_millis().div_ceil(1000).max(1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real time: tokio's paused clock needs the `test-util` feature, which the
    // crate doesn't enable. Lower bounds only, so a slow machine can't fail them.

    #[tokio::test]
    async fn reads_are_spaced_by_the_min_gap() {
        let p = Pacer::new();
        drop(p.turn(false).await.unwrap());
        let t0 = Instant::now();
        drop(p.turn(false).await.unwrap());
        assert!(Instant::now() - t0 >= MIN_GAP - Duration::from_millis(5));
    }

    #[tokio::test]
    async fn writes_are_spaced_by_the_write_gap_even_with_a_read_between() {
        let p = Pacer::new();
        let t0 = Instant::now();
        drop(p.turn(true).await.unwrap());
        drop(p.turn(false).await.unwrap());
        drop(p.turn(true).await.unwrap());
        assert!(Instant::now() - t0 >= WRITE_GAP.max(MIN_GAP * 2) - Duration::from_millis(5));
    }

    #[tokio::test]
    async fn only_one_turn_at_a_time() {
        let p = std::sync::Arc::new(Pacer::new());
        let first = p.turn(false).await.unwrap();
        let p2 = p.clone();
        let waiter = tokio::spawn(async move { p2.turn(false).await.map(|_| Instant::now()) });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!waiter.is_finished());
        let released = Instant::now();
        drop(first);
        let got = waiter.await.unwrap().unwrap();
        assert!(got - released >= MIN_GAP - Duration::from_millis(5));
    }

    #[tokio::test]
    async fn a_cooldown_refuses_until_it_passes() {
        let p = Pacer::new();
        p.turn(false).await.unwrap().cool_down(Duration::from_millis(2_500));
        assert_eq!(p.turn(false).await.err(), Some(3));
        p.turn_cooldown_for_test(Duration::from_millis(50)).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(p.turn(false).await.is_ok());
    }

    impl Pacer {
        async fn turn_cooldown_for_test(&self, wait: Duration) {
            self.gaps.lock().await.cooldown_until = Some(Instant::now() + wait);
        }
    }
}
