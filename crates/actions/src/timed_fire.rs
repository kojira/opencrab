//! Generic timed-turn routing for core-owned schedules.
//! A route is identified only by canonical core binding/session IDs; no transport descriptor,
//! platform token, or concrete lifecycle registry participates.

use crate::CallerIdentity;
use std::sync::{Arc, Mutex};

pub fn prompt_preview(prompt: &str) -> String {
    const MAX_CHARS: usize = 80;
    let one_line = prompt.replace(['\n', '\r'], " ");
    let head: String = one_line.chars().take(MAX_CHARS).collect();
    if one_line.chars().count() > MAX_CHARS {
        format!("{head}…")
    } else {
        head
    }
}

pub fn new_turn_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FireTarget {
    pub binding_id: String,
    pub session_id: String,
}

pub struct TimedFireRequest {
    pub binding_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub prompt: String,
    pub caller: CallerIdentity,
}

pub trait TimedFireSink: Send + Sync {
    fn fire_timed_turn(&self, req: TimedFireRequest);
}

#[derive(Default)]
pub struct TimedFireRouter {
    sink: Mutex<Option<Arc<dyn TimedFireSink>>>,
}

impl TimedFireRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_sink(&self, sink: Arc<dyn TimedFireSink>) {
        *self.sink.lock().unwrap() = Some(sink);
    }

    pub fn unregister_sink(&self) {
        *self.sink.lock().unwrap() = None;
    }

    pub fn resolve(&self) -> Option<Arc<dyn TimedFireSink>> {
        self.sink.lock().unwrap().clone()
    }

    pub fn has_live_sink(&self) -> bool {
        self.sink.lock().unwrap().is_some()
    }

    /// Resolve only the canonical generic binding authority. Exact aliases and global-address
    /// fallbacks have already converged to one binding row; no external address is parsed here.
    pub fn resolve_persisted_target(
        &self,
        conn: &rusqlite::Connection,
        session_id: &str,
        agent_id: &str,
    ) -> Option<FireTarget> {
        use opencrab_db::queries::CanonicalGateBindingLookup;
        match opencrab_db::queries::lookup_canonical_gate_binding(conn, session_id).ok()? {
            CanonicalGateBindingLookup::Match(binding) if binding.agent_id == agent_id => {
                Some(FireTarget {
                    binding_id: binding.binding_id,
                    session_id: session_id.to_string(),
                })
            }
            CanonicalGateBindingLookup::Match(_)
            | CanonicalGateBindingLookup::NotFound
            | CanonicalGateBindingLookup::Ambiguous => None,
        }
    }

    pub fn fire_target_hint(&self) -> &'static str {
        "ゲートに接続した会話"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingSink(Arc<AtomicUsize>);
    impl TimedFireSink for CountingSink {
        fn fire_timed_turn(&self, _req: TimedFireRequest) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn prompt_preview_is_one_line_and_truncated() {
        assert_eq!(prompt_preview("a\nb"), "a b");
        assert_eq!(prompt_preview(&"x".repeat(81)).chars().count(), 81);
    }

    #[test]
    fn generic_liveness_is_single_sink_without_kind_registry() {
        let router = TimedFireRouter::new();
        assert!(!router.has_live_sink());
        let count = Arc::new(AtomicUsize::new(0));
        router.register_sink(Arc::new(CountingSink(Arc::clone(&count))));
        router.resolve().unwrap().fire_timed_turn(TimedFireRequest {
            binding_id: "b".into(),
            session_id: "s".into(),
            agent_id: "a".into(),
            prompt: "p".into(),
            caller: CallerIdentity::Owner,
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
        router.unregister_sink();
        assert!(!router.has_live_sink());
    }
}
