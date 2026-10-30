//! D-1096: `interrupt` — stop the session's running turn and report how much of the
//! agent's last utterance reached the other side.

use serde_json::Value;

use super::{parse_session_key, require_str};
use crate::error::{ErrorCode, GateError};
use crate::ids::parse_request_id;

/// `interrupt`: answered with `ok`. With no running turn it only records `delivered`, if given.
#[derive(Debug, Clone)]
pub struct Interrupt {
    pub id: String,
    /// The gateway's opaque session key (D-1099).
    pub session_key: String,
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
        session_key: parse_session_key(obj)?,
        delivered,
    })
}
