//! D-1085: provisional (speculative) runs of a realtime said that is not committed yet.

use serde_json::{json, Value};

use super::{parse_uuid, require_str, FrameError, SaidCaller};

/// The inputs of a provisional run. The committed said must repeat them exactly (`author_id`,
/// `author_label`, `caller`, `system_context`, `agent_prelude`, `text`, realtime profile) to be
/// promoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionalRequest {
    pub provisional_id: String,
    pub author_id: String,
    pub author_label: Option<String>,
    pub caller: SaidCaller,
    pub system_context: Option<String>,
    pub agent_prelude: Option<String>,
    pub text: String,
}

pub fn provisional_frame(id: &str, binding_id: &str, request: &ProvisionalRequest) -> Value {
    let mut frame = json!({
        "id": id,
        "m": "provisional",
        "binding_id": binding_id,
        "provisional_id": request.provisional_id,
        "author_id": request.author_id,
        "caller": request.caller,
        "model_profile": "realtime",
        "text": request.text,
    });
    if let Some(label) = &request.author_label {
        frame["author_label"] = json!(label);
    }
    if let Some(system) = &request.system_context {
        frame["system_context"] = json!(system);
    }
    if let Some(prelude) = &request.agent_prelude {
        frame["agent_prelude"] = json!(prelude);
    }
    frame
}

pub fn provisional_cancel_frame(id: &str, binding_id: &str, provisional_id: &str) -> Value {
    json!({
        "id": id,
        "m": "provisional_cancel",
        "binding_id": binding_id,
        "provisional_id": provisional_id,
    })
}

/// D-1096: stop the binding session's running turn. `delivered` is the part of the agent's last
/// utterance that reached the other side (`Some("")`: none of it); core records it.
pub fn interrupt_frame(id: &str, binding_id: &str, delivered: Option<&str>) -> Value {
    let mut frame = json!({"id": id, "m": "interrupt", "binding_id": binding_id});
    if let Some(delivered) = delivered {
        frame["delivered"] = json!(delivered);
    }
    frame
}

/// Core's id-less report, written before the turn's say: whether the said that carried
/// `provisional_id` reuses that run (`promoted`) or runs a normal turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionalResult {
    pub binding_id: String,
    pub provisional_id: String,
    pub promoted: bool,
}

pub(super) fn parse_provisional_result(obj: &Value) -> Result<ProvisionalResult, FrameError> {
    Ok(ProvisionalResult {
        binding_id: parse_uuid(&require_str(obj, "binding_id")?)?,
        provisional_id: parse_uuid(&require_str(obj, "provisional_id")?)?,
        promoted: obj
            .get("promoted")
            .and_then(Value::as_bool)
            .ok_or(FrameError::BadRequest)?,
    })
}
