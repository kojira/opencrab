//! D-1085: provisional (speculative) runs of a realtime said that has not been committed yet.

use serde_json::{json, Value};

use super::{
    nonempty_str, optional_bounded_text, parse_agent_prelude, parse_author_label,
    parse_model_profile, parse_said_caller, require_str, SaidCaller,
};
use crate::error::{ErrorCode, GateError};
use crate::ids::{parse_request_id, parse_uuid};

/// `provisional`: start a provisional run for text the gateway expects to commit as a said.
/// Only the realtime profile is accepted. Nothing is recorded until a said promotes it.
#[derive(Debug, Clone)]
pub struct Provisional {
    pub id: String,
    pub binding_id: String,
    pub provisional_id: String,
    pub author_id: String,
    pub author_label: Option<String>,
    pub caller: SaidCaller,
    pub system_context: Option<String>,
    pub agent_prelude: Option<String>,
    pub text: String,
}

/// `provisional_cancel`: abort a provisional run. Idempotent.
#[derive(Debug, Clone)]
pub struct ProvisionalCancel {
    pub id: String,
    pub binding_id: String,
    pub provisional_id: String,
}

pub(super) fn parse_provisional(obj: &Value) -> Result<Provisional, GateError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let provisional_id = parse_uuid(&require_str(obj, "provisional_id")?)?;
    let author_id = nonempty_str(obj, "author_id")?;
    let author_label = parse_author_label(obj.get("author_label"))?;
    let caller = parse_said_caller(obj.get("caller"))?;
    let system_context = optional_bounded_text(obj.get("system_context"), 16_384)?;
    if parse_model_profile(obj.get("model_profile"))?
        != opencrab_actions::TurnModelProfile::Realtime
    {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    let agent_prelude = parse_agent_prelude(obj.get("agent_prelude"))?;
    let text = nonempty_str(obj, "text")?;
    Ok(Provisional {
        id,
        binding_id,
        provisional_id,
        author_id,
        author_label,
        caller,
        system_context,
        agent_prelude,
        text,
    })
}

pub(super) fn parse_provisional_cancel(obj: &Value) -> Result<ProvisionalCancel, GateError> {
    Ok(ProvisionalCancel {
        id: parse_request_id(&require_str(obj, "id")?)?,
        binding_id: parse_uuid(&require_str(obj, "binding_id")?)?,
        provisional_id: parse_uuid(&require_str(obj, "provisional_id")?)?,
    })
}

/// Whether the said carrying `provisional_id` reuses that provisional run (`promoted`) or runs a
/// normal turn. It has no `id`; core writes it before the turn's delivery.
pub fn provisional_result_frame(binding_id: &str, provisional_id: &str, promoted: bool) -> Value {
    json!({
        "m": "provisional_result",
        "binding_id": binding_id,
        "provisional_id": provisional_id,
        "promoted": promoted,
    })
}
