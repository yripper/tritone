use super::fake::*;
use super::*;

#[test]
fn thread_rng_stays_in_range() {
    for _ in 0..200 {
        let g = ThreadRng.gap_ms(20_000, 60_000);
        assert!((20_000..=60_000).contains(&g));
    }
}

#[tokio::test]
async fn fake_timer_sleep_moves_time_forward_never_back() {
    let t = FakeTimer::at(1_000);
    t.sleep_until(5_000).await;
    assert_eq!(t.now_ms(), 5_000);
    t.sleep_until(2_000).await;
    assert_eq!(t.now_ms(), 5_000);
    assert_eq!(t.now_secs(), 5);
}

#[tokio::test]
async fn system_timer_returns_at_once_for_past_deadlines() {
    let t = SystemTimer;
    let before = t.now_ms();
    t.sleep_until(before - 10_000).await;
    assert!(t.now_ms() - before < 1_000);
}

#[test]
fn now_ms_reads_the_timer() {
    let t = FakeTimer::at(42);
    assert_eq!(now_ms(&t), 42);
}
