//! D-1083: core→gate `partial` frame and the say's `stream_id`.

use serde_json::Value;

use super::{nonempty_str, parse_uuid, require_str, FrameError};

/// Cumulative released text of one partial stream. The frame has no `id`, so the gateway never
/// replies to it. A later partial or the say carrying the same `stream_id` supersedes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial {
    pub binding_id: String,
    pub stream_id: String,
    pub text: String,
}

pub(super) fn parse_partial(obj: &Value) -> Result<Partial, FrameError> {
    Ok(Partial {
        binding_id: parse_uuid(&require_str(obj, "binding_id")?)?,
        stream_id: nonempty_str(obj, "stream_id")?,
        text: nonempty_str(obj, "text")?,
    })
}

/// The say payload's additive `stream_id`: the partial stream this say completes. Missing or
/// non-string values are None, so the say is handled as before.
pub fn say_stream_id(payload: &Value) -> Option<&str> {
    match payload.get("stream_id") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}
