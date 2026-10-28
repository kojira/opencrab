//! D-1096: `interrupt` — stop the binding session's running turn and report how much of the
//! agent's last utterance reached the other side.

use serde_json::Value;

use super::require_str;
use crate::error::{ErrorCode, GateError};
use crate::ids::{parse_request_id, parse_uuid};

/// `interrupt`: answered with `ok`. With no running turn it only records `delivered`, if given.
#[derive(Debug, Clone)]
pub struct Interrupt {
    pub id: String,
    pub binding_id: String,
    /// The part of the agent's last utterance that reached the other side. `Some("")`: none of it.
    pub delivered: Option<String>,
}

pub(super) fn parse_interrupt(obj: &Value) -> Result<Interrupt, GateError> {
    let delivered = match obj.get("delivered") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return Err(GateError::new(ErrorCode::BadRequest)),
    };
    Ok(Interrupt {
        id: parse_request_id(&require_str(obj, "id")?)?,
        binding_id: parse_uuid(&require_str(obj, "binding_id")?)?,
        delivered,
    })
}
