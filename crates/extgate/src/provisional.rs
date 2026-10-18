//! D-1085: provisional (speculative) runs of a realtime said the gateway has not committed yet.
//!
//! A `provisional` frame starts a lightweight run on a snapshot of the session (speech rows up to
//! the session's high-water mark, then the provisional user line and agent prelude). Nothing is
//! recorded. When the committed said carries the same `provisional_id` and the same inputs, and
//! the session gained no other row since the snapshot, the turn reuses the run's result
//! ([`promote::take_promotion`]); otherwise the run is aborted and the normal turn runs.
//! §12: the run waits on its promotion gate before any tool outside the read-only set; the
//! promotion opens the gate and records the rows the run held.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use opencrab_core::{EngineResult, PromotionGate};
use tokio::task::JoinHandle;

use crate::partial_stream::PartialStreamSlot;
use crate::protocol::SaidCaller;

pub(crate) mod promote;
pub(crate) mod run;

/// How long an unpromoted provisional run is kept.
pub const PROVISIONAL_TTL: Duration = Duration::from_secs(10);
/// At most this many provisional runs per binding (the oldest is aborted).
pub const PROVISIONALS_PER_BINDING: usize = 4;

pub(crate) type ProvisionalTask = JoinHandle<Option<anyhow::Result<EngineResult>>>;

/// The inputs a said must repeat exactly to promote a provisional run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvisionalInputs {
    pub author_id: String,
    pub author_label: Option<String>,
    pub caller: SaidCaller,
    pub system_context: String,
    pub agent_prelude: Option<String>,
    pub text: String,
}

struct Entry {
    serial: u64,
    started: Instant,
    session_id: String,
    high_water: i64,
    inputs: ProvisionalInputs,
    stream: PartialStreamSlot,
    gate: PromotionGate,
    task: ProvisionalTask,
}

/// A provisional run taken out of the registry by its said.
pub(crate) struct TakenProvisional {
    pub session_id: String,
    pub high_water: i64,
    pub inputs: ProvisionalInputs,
    pub stream: PartialStreamSlot,
    pub gate: PromotionGate,
    pub task: ProvisionalTask,
}

/// Provisional runs by `(binding_id, provisional_id)`.
#[derive(Default)]
pub struct ProvisionalRegistry {
    entries: Mutex<HashMap<(String, String), Entry>>,
    next_serial: Mutex<u64>,
}

impl ProvisionalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a started run. Aborts the run it replaces (same id, or the same author on the
    /// binding), expired runs, and the binding's oldest run beyond [`PROVISIONALS_PER_BINDING`].
    /// Returns the entry serial for [`ProvisionalRegistry::expire`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert(
        &self,
        binding_id: &str,
        provisional_id: &str,
        session_id: String,
        high_water: i64,
        inputs: ProvisionalInputs,
        stream: PartialStreamSlot,
        gate: PromotionGate,
        task: ProvisionalTask,
    ) -> u64 {
        let serial = {
            let mut next = self.next_serial.lock().expect("provisional serial lock");
            *next += 1;
            *next
        };
        let mut entries = self.entries.lock().expect("provisional registry lock");
        entries.retain(|(binding, id), entry| {
            let keep = entry.started.elapsed() < PROVISIONAL_TTL
                && !(binding == binding_id
                    && (id == provisional_id || entry.inputs.author_id == inputs.author_id));
            if !keep {
                entry.gate.abort();
                entry.task.abort();
            }
            keep
        });
        let mut on_binding: Vec<(u64, String)> = entries
            .iter()
            .filter(|((binding, _), _)| binding == binding_id)
            .map(|((_, id), entry)| (entry.serial, id.clone()))
            .collect();
        on_binding.sort();
        let excess = (on_binding.len() + 1).saturating_sub(PROVISIONALS_PER_BINDING);
        for (_, id) in on_binding.into_iter().take(excess) {
            if let Some(entry) = entries.remove(&(binding_id.to_string(), id)) {
                entry.gate.abort();
                entry.task.abort();
            }
        }
        entries.insert(
            (binding_id.to_string(), provisional_id.to_string()),
            Entry {
                serial,
                started: Instant::now(),
                session_id,
                high_water,
                inputs,
                stream,
                gate,
                task,
            },
        );
        serial
    }

    /// Takes the run out of the registry; `None` when it is unknown, replaced or expired.
    pub(crate) fn take(&self, binding_id: &str, provisional_id: &str) -> Option<TakenProvisional> {
        let entry = self
            .entries
            .lock()
            .expect("provisional registry lock")
            .remove(&(binding_id.to_string(), provisional_id.to_string()))?;
        if entry.started.elapsed() >= PROVISIONAL_TTL {
            entry.gate.abort();
            entry.task.abort();
            return None;
        }
        Some(TakenProvisional {
            session_id: entry.session_id,
            high_water: entry.high_water,
            inputs: entry.inputs,
            stream: entry.stream,
            gate: entry.gate,
            task: entry.task,
        })
    }

    /// Aborts and forgets the run; returns its task so the caller can wait for the abort.
    pub(crate) fn remove(&self, binding_id: &str, provisional_id: &str) -> Option<ProvisionalTask> {
        let entry = self
            .entries
            .lock()
            .expect("provisional registry lock")
            .remove(&(binding_id.to_string(), provisional_id.to_string()))?;
        entry.gate.abort();
        entry.task.abort();
        Some(entry.task)
    }

    /// Aborts and forgets the run without waiting.
    pub(crate) fn discard(&self, binding_id: &str, provisional_id: &str) {
        let _ = self.remove(binding_id, provisional_id);
    }

    /// TTL reaper: aborts the run when it is still the entry registered as `serial`.
    pub(crate) fn expire(&self, binding_id: &str, provisional_id: &str, serial: u64) {
        let mut entries = self.entries.lock().expect("provisional registry lock");
        let key = (binding_id.to_string(), provisional_id.to_string());
        if entries
            .get(&key)
            .is_some_and(|entry| entry.serial == serial)
        {
            if let Some(entry) = entries.remove(&key) {
                entry.gate.abort();
                entry.task.abort();
            }
        }
    }

    /// Number of registered runs (tests and diagnostics).
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("provisional registry lock")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
