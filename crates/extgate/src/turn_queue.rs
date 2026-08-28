//! record 後の session turn queue（DESIGN-NOSTRGATE §4 #6 / §6 #6）。
//! cap 32。溢れは turn を積まず計測する。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

pub const SESSION_QUEUE_CAPACITY: usize = 32;

type TurnJob = Pin<Box<dyn Future<Output = ()> + Send>>;

pub struct SessionTurnQueues {
    capacity: usize,
    reserved: Mutex<HashMap<String, usize>>,
    queues: Mutex<HashMap<String, mpsc::Sender<TurnJob>>>,
    dropped: AtomicU64,
}

impl SessionTurnQueues {
    pub fn new() -> Self {
        Self {
            capacity: SESSION_QUEUE_CAPACITY,
            reserved: Mutex::new(HashMap::new()),
            queues: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn has_room(&self, session_id: &str) -> bool {
        let reserved = self.reserved.lock().expect("turn queue reserved");
        reserved.get(session_id).copied().unwrap_or(0) < self.capacity
    }

    pub fn note_dropped(&self) -> u64 {
        self.dropped.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn try_reserve(&self, session_id: &str) -> bool {
        let mut reserved = self.reserved.lock().expect("turn queue reserved");
        let n = reserved.get(session_id).copied().unwrap_or(0);
        if n >= self.capacity {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        reserved.insert(session_id.to_string(), n + 1);
        true
    }

    fn release(&self, session_id: &str) {
        let mut reserved = self.reserved.lock().expect("turn queue reserved");
        let Some(n) = reserved.get_mut(session_id) else {
            return;
        };
        *n = n.saturating_sub(1);
        if *n == 0 {
            reserved.remove(session_id);
        }
    }

    pub fn submit<F>(self: &Arc<Self>, session_id: &str, job: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let session_key = session_id.to_string();
        let queues = self.clone();
        let wrapped: TurnJob = Box::pin(async move {
            job.await;
            queues.release(&session_key);
        });
        self.send(session_id, wrapped);
    }

    fn send(self: &Arc<Self>, session_id: &str, job: TurnJob) {
        let mut queues = self.queues.lock().expect("turn queue");
        let existing = queues.get(session_id).cloned();
        let job = match existing {
            Some(tx) => match tx.try_send(job) {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Full(_job)) => {
                    self.release(session_id);
                    let dropped = self.note_dropped();
                    tracing::warn!(
                        session_id,
                        capacity = self.capacity,
                        dropped_total = dropped,
                        "extgate: session turn queue full"
                    );
                    return;
                }
                Err(mpsc::error::TrySendError::Closed(job)) => {
                    queues.remove(session_id);
                    job
                }
            },
            None => job,
        };
        let (tx, rx) = mpsc::channel(self.capacity);
        if tx.try_send(job).is_err() {
            self.release(session_id);
            tracing::error!(session_id, "extgate: session turn queue init failed");
            return;
        }
        queues.insert(session_id.to_string(), tx);
        let this = self.clone();
        let session_id = session_id.to_string();
        tokio::spawn(async move { this.run_consumer(rx, session_id).await });
    }

    async fn run_consumer(self: Arc<Self>, mut rx: mpsc::Receiver<TurnJob>, session_id: String) {
        loop {
            let job = match rx.try_recv() {
                Ok(job) => job,
                Err(_) => match self.retire_or_take(&session_id, &mut rx) {
                    Some(job) => job,
                    None => return,
                },
            };
            job.await;
        }
    }

    fn retire_or_take(
        &self,
        session_id: &str,
        rx: &mut mpsc::Receiver<TurnJob>,
    ) -> Option<TurnJob> {
        let mut queues = self.queues.lock().expect("turn queue");
        match rx.try_recv() {
            Ok(job) => Some(job),
            Err(_) => {
                queues.remove(session_id);
                None
            }
        }
    }
}

impl Default for SessionTurnQueues {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::{oneshot, Notify};

    #[test]
    fn capacity_is_32() {
        assert_eq!(SESSION_QUEUE_CAPACITY, 32);
    }

    #[tokio::test]
    async fn reserve_rejects_at_capacity() {
        let q = SessionTurnQueues::new();
        for _ in 0..SESSION_QUEUE_CAPACITY {
            assert!(q.try_reserve("s1"));
        }
        assert!(!q.has_room("s1"));
        assert!(!q.try_reserve("s1"));
        assert_eq!(q.dropped(), 1);
        assert!(q.has_room("s2"));
    }

    #[tokio::test]
    async fn queued_job_runs_after_current() {
        let q = Arc::new(SessionTurnQueues::new());
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let first_entered = Arc::new(Notify::new());
        let entered = first_entered.clone();
        let order = Arc::new(AtomicUsize::new(0));
        let a = order.clone();
        assert!(q.try_reserve("s"));
        q.submit("s", async move {
            entered.notify_one();
            let _ = release_rx.await;
            a.store(1, Ordering::SeqCst);
        });
        first_entered.notified().await;
        let b = order.clone();
        assert!(q.try_reserve("s"));
        q.submit("s", async move {
            b.store(2, Ordering::SeqCst);
        });
        assert_eq!(order.load(Ordering::SeqCst), 0);
        release_tx.send(()).unwrap();
        for _ in 0..50 {
            if order.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(order.load(Ordering::SeqCst), 2);
    }
}
