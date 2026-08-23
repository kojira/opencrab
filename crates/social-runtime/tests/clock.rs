use opencrab_social_runtime::{Clock, FakeClock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn fake_clock_wakes_only_at_the_explicit_deadline() {
    let clock = FakeClock::new();
    let start = clock.now();
    let wall_start = clock.now_wall_nanos();
    let woke = Arc::new(AtomicBool::new(false));
    let task_clock = clock.clone();
    let task_woke = woke.clone();
    let task = tokio::spawn(async move {
        task_clock
            .sleep_until(start + Duration::from_secs(60))
            .await;
        task_woke.store(true, Ordering::SeqCst);
    });

    tokio::task::yield_now().await;
    assert!(!woke.load(Ordering::SeqCst));

    clock.advance(Duration::from_millis(59_999));
    tokio::task::yield_now().await;
    assert!(!woke.load(Ordering::SeqCst));

    clock.advance(Duration::from_millis(1));
    task.await.expect("fake-clock sleeper task");
    assert!(woke.load(Ordering::SeqCst));
    assert_eq!(clock.now(), start + Duration::from_secs(60));
    assert_eq!(
        clock.now_wall_nanos(),
        wall_start + Duration::from_secs(60).as_nanos() as i64
    );
}
