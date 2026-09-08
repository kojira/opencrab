//! 試験用 barrier。armed でなければ park は即 return。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct Slot {
    armed: AtomicBool,
    parked: AtomicBool,
    released: AtomicBool,
}

impl Slot {
    const fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            parked: AtomicBool::new(false),
            released: AtomicBool::new(false),
        }
    }
}

static AFTER_COMMIT: Slot = Slot::new();
static AFTER_PENDING: Slot = Slot::new();
static BEFORE_HTTP_READY: Slot = Slot::new();

fn slot(name: &str) -> &'static Slot {
    match name {
        "after_commit" => &AFTER_COMMIT,
        "after_pending" => &AFTER_PENDING,
        "before_http_ready" => &BEFORE_HTTP_READY,
        other => panic!("unknown race barrier {other}"),
    }
}

pub fn arm(name: &str) {
    let s = slot(name);
    s.released.store(false, Ordering::SeqCst);
    s.parked.store(false, Ordering::SeqCst);
    s.armed.store(true, Ordering::SeqCst);
}

pub fn disarm_all() {
    for name in ["after_commit", "after_pending", "before_http_ready"] {
        let s = slot(name);
        s.armed.store(false, Ordering::SeqCst);
        s.released.store(true, Ordering::SeqCst);
        s.parked.store(false, Ordering::SeqCst);
    }
}

pub async fn park(name: &str) {
    let s = slot(name);
    if !s.armed.load(Ordering::SeqCst) {
        return;
    }
    s.parked.store(true, Ordering::SeqCst);
    loop {
        if s.released.load(Ordering::SeqCst) || !s.armed.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub fn wait_parked(name: &str, timeout: Duration) -> bool {
    let s = slot(name);
    let start = Instant::now();
    while start.elapsed() < timeout {
        if s.parked.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

pub fn release(name: &str) {
    let s = slot(name);
    s.released.store(true, Ordering::SeqCst);
    s.armed.store(false, Ordering::SeqCst);
}
