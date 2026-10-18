//! D-1085 §12: the promotion signal of a provisional (speculative) run.
//!
//! A provisional run may only run the tools the caller declares read-only. Before any other tool
//! the engine waits on this gate ([`PromotionGate::promoted`]). The owner of the run promotes it
//! (the committed input matched) or aborts it. Hooks registered with
//! [`PromotionGate::on_promoted`] run once, at promotion, before any waiting run continues.

use std::sync::{Arc, Mutex};

use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateState {
    Pending,
    Promoted,
    Aborted,
}

type PromotedHook = Box<dyn FnOnce() + Send>;

struct Inner {
    state: watch::Sender<GateState>,
    /// Hooks waiting for the promotion. The state changes under this lock.
    hooks: Mutex<Vec<PromotedHook>>,
}

/// Shared promoted/aborted signal; clones observe the same gate.
#[derive(Clone)]
pub struct PromotionGate {
    inner: Arc<Inner>,
}

impl Default for PromotionGate {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PromotionGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PromotionGate")
            .field(&*self.inner.state.borrow())
            .finish()
    }
}

impl PromotionGate {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: watch::Sender::new(GateState::Pending),
                hooks: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Opens the gate: runs the registered hooks, then releases the waiting run. No effect once
    /// promoted or aborted. The hooks run under the gate lock, so a concurrent
    /// [`PromotionGate::on_promoted`] runs after them. A hook must not use the gate.
    pub fn promote(&self) {
        let mut hooks = self.inner.hooks.lock().expect("promotion gate lock");
        if *self.inner.state.borrow() != GateState::Pending {
            return;
        }
        for hook in std::mem::take(&mut *hooks) {
            hook();
        }
        self.inner.state.send_replace(GateState::Promoted);
    }

    /// Closes the gate for good: drops the hooks and makes the waiting run fail. No effect once
    /// promoted or aborted.
    pub fn abort(&self) {
        let mut hooks = self.inner.hooks.lock().expect("promotion gate lock");
        if *self.inner.state.borrow() == GateState::Pending {
            hooks.clear();
            self.inner.state.send_replace(GateState::Aborted);
        }
    }

    /// Runs `hook` at promotion, or now when the gate is already promoted. Dropped on abort.
    pub fn on_promoted(&self, hook: impl FnOnce() + Send + 'static) {
        let mut hooks = self.inner.hooks.lock().expect("promotion gate lock");
        match *self.inner.state.borrow() {
            GateState::Pending => hooks.push(Box::new(hook)),
            GateState::Promoted => {
                drop(hooks);
                hook();
            }
            GateState::Aborted => {}
        }
    }

    pub fn is_promoted(&self) -> bool {
        *self.inner.state.borrow() == GateState::Promoted
    }

    /// Waits until the gate is decided: `true` when promoted, `false` when aborted.
    pub async fn promoted(&self) -> bool {
        let mut rx = self.inner.state.subscribe();
        loop {
            match *rx.borrow_and_update() {
                GateState::Promoted => return true,
                GateState::Aborted => return false,
                GateState::Pending => {}
            }
            if rx.changed().await.is_err() {
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn promote_runs_hooks_once_then_releases_waiters() {
        let gate = PromotionGate::new();
        let ran = Arc::new(AtomicUsize::new(0));
        gate.on_promoted({
            let ran = ran.clone();
            move || {
                ran.fetch_add(1, Ordering::SeqCst);
            }
        });
        let waiter = tokio::spawn({
            let gate = gate.clone();
            async move { gate.promoted().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        gate.promote();
        gate.promote();
        assert!(waiter.await.unwrap());
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        gate.on_promoted({
            let ran = ran.clone();
            move || {
                ran.fetch_add(10, Ordering::SeqCst);
            }
        });
        assert_eq!(ran.load(Ordering::SeqCst), 11, "a late hook runs at once");
        gate.abort();
        assert!(gate.is_promoted());
    }

    #[tokio::test]
    async fn abort_drops_hooks_and_fails_waiters() {
        let gate = PromotionGate::new();
        let ran = Arc::new(AtomicUsize::new(0));
        gate.on_promoted({
            let ran = ran.clone();
            move || {
                ran.fetch_add(1, Ordering::SeqCst);
            }
        });
        gate.abort();
        gate.promote();
        assert!(!gate.promoted().await);
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(!gate.is_promoted());
    }
}
