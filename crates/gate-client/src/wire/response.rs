//! Parsing of the `ok` / `err` response frames.

use serde_json::Value;

use super::{parse_request_id, require_str, FrameError, WireResponse};

pub(super) fn parse_response(obj: &Value, m: &str) -> Result<WireResponse, FrameError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    if m == "ok" {
        let seq = match obj.get("seq") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::Number(n)) => {
                let v = n.as_i64().ok_or(FrameError::BadRequest)?;
                if v <= 0 {
                    return Err(FrameError::BadRequest);
                }
                Some(Some(v))
            }
            Some(_) => return Err(FrameError::BadRequest),
        };
        Ok(WireResponse {
            id,
            ok: true,
            seq,
            result: obj.get("result").cloned(),
            code: None,
            detail: None,
            message: None,
        })
    } else {
        let code = require_str(obj, "code")?;
        let detail = match obj.get("detail") {
            Some(Value::Null) | None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err(FrameError::BadRequest),
        };
        let message = match obj.get("message") {
            None => None,
            Some(Value::String(message)) => Some(message.clone()),
            Some(_) => return Err(FrameError::BadRequest),
        };
        if obj.get("detail").is_none() && message.is_none() {
            return Err(FrameError::BadRequest);
        }
        Ok(WireResponse {
            id,
            ok: false,
            seq: None,
            result: None,
            code: Some(code),
            detail,
            message,
        })
    }
}
