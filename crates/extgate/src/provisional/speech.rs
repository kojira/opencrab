//! D-1085 §13.2: continuation speech of a provisional run, held until its promotion.
//!
//! Before the said promotes the run, each continuation speech is queued for delivery and its
//! speech row waits on the promotion gate, in order with the run's other held rows. The said's
//! turn then [`HeldSpeech::release`]s: the queued says go out in order, and later speech is
//! delivered (and recorded) at once like a normal turn's. A run that is not promoted drops the
//! queue with its gate.

use std::sync::{Arc, Mutex};

use opencrab_actions::run_request::ContinuationSpeechHook;
use opencrab_core::PromotionGate;

use crate::partial_stream::PartialStreamSlot;
use crate::registry::ExtgateState;

/// Where the says go once the run is promoted.
#[derive(Clone)]
pub(crate) struct SayTarget {
    pub state: Arc<ExtgateState>,
    pub instance_id: String,
    pub binding_id: String,
    pub agent_id: String,
    pub session_id: String,
    pub reply_target: Option<String>,
    /// The turn's last continuation say (for its `completed_target`).
    pub latest: Arc<Mutex<Option<String>>>,
}

#[derive(Default)]
struct HeldState {
    /// `(speech, stream_id)` waiting for the release.
    queue: Vec<(String, Option<String>)>,
    target: Option<SayTarget>,
}

/// The held continuation speech of one provisional run.
#[derive(Clone, Default)]
pub(crate) struct HeldSpeech(Arc<tokio::sync::Mutex<HeldState>>);

impl HeldSpeech {
    pub fn new() -> Self {
        Self::default()
    }

    /// The run's continuation-speech hook. `stream` is the run's partial stream.
    pub fn hook(
        &self,
        db: opencrab_db::Db,
        gate: PromotionGate,
        stream: PartialStreamSlot,
        agent_id: String,
        session_id: String,
    ) -> ContinuationSpeechHook {
        let held = self.clone();
        Arc::new(move |speech: String| {
            let held = held.clone();
            let db = db.clone();
            let gate = gate.clone();
            let agent_id = agent_id.clone();
            let session_id = session_id.clone();
            let stream_id = stream.take_for_say();
            Box::pin(async move {
                let mut state = held.0.lock().await;
                if let Some(target) = state.target.clone() {
                    drop(state);
                    return deliver(&target, &speech, stream_id.as_deref(), true)
                        .await
                        .map_err(|code| {
                            anyhow::anyhow!("extgate provisional intermediate say failed: {code}")
                        });
                }
                let row = crate::delivery::say_speech_row(&agent_id, &session_id, &speech);
                gate.on_promoted(move || match db.lock() {
                    Ok(conn) => {
                        if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &row) {
                            tracing::error!(%session_id, "held continuation speech row failed: {e}");
                        }
                    }
                    Err(_) => tracing::error!(%session_id, "held continuation speech db lock"),
                });
                state.queue.push((speech, stream_id));
                Ok(())
            })
        })
    }

    /// After the promotion: delivers the queued says in order; later speech goes out at once.
    pub async fn release(&self, target: SayTarget) {
        let mut state = self.0.lock().await;
        for (speech, stream_id) in std::mem::take(&mut state.queue) {
            if let Err(code) = deliver(&target, &speech, stream_id.as_deref(), false).await {
                tracing::warn!(
                    session_id = %target.session_id,
                    code,
                    "held continuation say failed"
                );
            }
        }
        state.target = Some(target);
    }
}

async fn deliver(
    target: &SayTarget,
    speech: &str,
    stream_id: Option<&str>,
    record: bool,
) -> Result<(), &'static str> {
    let t = target;
    let delivered = if record {
        crate::delivery::deliver_intermediate_say(
            &t.state,
            &t.instance_id,
            &t.binding_id,
            &t.agent_id,
            &t.session_id,
            speech,
            t.reply_target.as_deref(),
            stream_id,
        )
        .await
    } else {
        crate::delivery::deliver_recorded_say(
            &t.state,
            &t.instance_id,
            &t.binding_id,
            &t.agent_id,
            &t.session_id,
            speech,
            t.reply_target.as_deref(),
            stream_id,
        )
        .await
    };
    let delivery_id = delivered.map_err(|e| e.code.as_str())?;
    *target.latest.lock().expect("continuation say id lock") = Some(delivery_id);
    Ok(())
}
