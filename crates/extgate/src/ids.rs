//! 識別子と digest。V3 §2。

use sha2::{Digest, Sha256};

use crate::error::{ErrorCode, GateError};

/// `session_id_for_binding(binding_id)` = `"extgate-" + binding_id`。
pub fn session_id_for_binding(binding_id: &str) -> String {
    format!("extgate-{binding_id}")
}

/// UTC Unix nanoseconds。範囲外は fail-loud。
pub fn now_nanos() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .expect("timestamp_nanos out of range")
}

/// canonical lowercase UUID text。入力が byte-equal でなければ拒否。
pub fn parse_uuid(raw: &str) -> Result<String, GateError> {
    let parsed = uuid::Uuid::parse_str(raw).map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    let canonical = parsed.to_string();
    if canonical != raw {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    Ok(canonical)
}

/// request `id` は UTF-8 で 1..128 byte。
pub fn parse_request_id(raw: &str) -> Result<String, GateError> {
    let n = raw.len();
    if n == 0 || n > 128 {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    Ok(raw.to_string())
}

/// `config_b64` を RFC 4648 standard padded base64 decode し、SHA-256 lowerhex 64。
pub fn decode_config_b64(config_b64: &str) -> Result<Vec<u8>, GateError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(config_b64)
        .map_err(|_| GateError::new(ErrorCode::BadRequest))
}

pub fn config_digest(config_bytes: &[u8]) -> String {
    let hash = Sha256::digest(config_bytes);
    hex_lower(&hash)
}

pub fn config_digest_from_b64(config_b64: &str) -> Result<String, GateError> {
    Ok(config_digest(&decode_config_b64(config_b64)?))
}

/// digest は lowerhex 64 文字。
pub fn parse_digest(raw: &str) -> Result<String, GateError> {
    if raw.len() != 64 || !raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    Ok(raw.to_string())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// bind の request id。
pub fn bind_request_id(binding_id: &str) -> String {
    format!("bind:{binding_id}")
}
