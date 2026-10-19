//! D-1083: partial text of a realtime-profile turn, written to the gateway before the say.
//!
//! The turn owns one [`PartialStreamSlot`]. The engine's partial hook updates it and writes
//! `partial` frames; every say of the turn takes it, and carries `stream_id` only when that
//! stream released text.

use std::sync::{Arc, Mutex};

use crate::protocol::{partial_frame, write_json};
use crate::registry::ExtgateState;

#[derive(Debug, Default)]
struct SlotState {
    stream_id: String,
    released: bool,
}

/// Stream of the current LLM call of one turn.
#[derive(Debug, Clone, Default)]
pub struct PartialStreamSlot(Arc<Mutex<Option<SlotState>>>);

impl PartialStreamSlot {
    pub fn new() -> Self {
        Self::default()
    }

    fn begin(&self, stream_id: String) {
        *self.0.lock().expect("partial stream slot lock") = Some(SlotState {
            stream_id,
            released: false,
        });
    }

    fn mark_released(&self, stream_id: &str) {
        if let Some(slot) = self
            .0
            .lock()
            .expect("partial stream slot lock")
            .as_mut()
            .filter(|slot| slot.stream_id == stream_id)
        {
            slot.released = true;
        }
    }

    /// Empties the slot and returns the stream id for a say, when that stream released text.
    pub fn take_for_say(&self) -> Option<String> {
        self.0
            .lock()
            .expect("partial stream slot lock")
            .take()
            .filter(|slot| slot.released)
            .map(|slot| slot.stream_id)
    }
}

/// The engine hook of a realtime-profile turn. Write errors are logged and never fail the turn.
/// D-1085: the partials of a provisional run carry its `provisional_id`.
pub fn partial_text_hook(
    state: Arc<ExtgateState>,
    instance_id: String,
    binding_id: String,
    slot: PartialStreamSlot,
    provisional_id: Option<String>,
) -> opencrab_core::PartialTextHook {
    let turn_started = std::time::Instant::now();
    Arc::new(move |event| {
        let state = Arc::clone(&state);
        let instance_id = instance_id.clone();
        let binding_id = binding_id.clone();
        let slot = slot.clone();
        let provisional_id = provisional_id.clone();
        Box::pin(async move {
            match event {
                opencrab_core::PartialTextEvent::Begin { stream_id } => slot.begin(stream_id),
                opencrab_core::PartialTextEvent::Text { stream_id, text } => {
                    slot.mark_released(&stream_id);
                    write_partial(
                        &state,
                        &instance_id,
                        &binding_id,
                        &stream_id,
                        &text,
                        provisional_id.as_deref(),
                    )
                    .await;
                    tracing::debug!(
                        binding_id = %binding_id,
                        stream_id = %stream_id,
                        text_chars = text.chars().count(),
                        since_turn_start_ms = turn_started.elapsed().as_millis() as u64,
                        "partial frame written"
                    );
                }
            }
        })
    })
}

async fn write_partial(
    state: &Arc<ExtgateState>,
    instance_id: &str,
    binding_id: &str,
    stream_id: &str,
    text: &str,
    provisional_id: Option<&str>,
) {
    let writer = {
        let Ok(reg) = state.lock_registry() else {
            return;
        };
        let Some(live) = reg.get(instance_id) else {
            return;
        };
        if !live.acknowledged.contains(binding_id) {
            return;
        }
        live.writer.clone()
    };
    let frame = partial_frame(binding_id, stream_id, text, provisional_id);
    if let Err(e) = write_json(&writer, &frame).await {
        tracing::warn!(
            code = e.code.as_str(),
            "partial frame write failed; ignored"
        );
    }
}

/// D-1085: the writer of a live instance, when it acknowledged `binding_id`.
pub(crate) fn acknowledged_writer(
    state: &ExtgateState,
    instance_id: &str,
    binding_id: &str,
) -> Option<Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>> {
    let reg = state.lock_registry().ok()?;
    let live = reg.get(instance_id)?;
    live.acknowledged
        .contains(binding_id)
        .then(|| live.writer.clone())
}
