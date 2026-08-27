//! V3 §3 の frame と message。core crate の DTO は使わない。

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::json::{parse_object_no_dup, JsonError};

/// LF 込み上限。
pub const MAX_FRAME: usize = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    TooLarge,
    Eof,
    Io,
    BadRequest,
}

pub async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Vec<u8>, FrameError> {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match reader.read_exact(&mut byte).await {
            Ok(_) => {
                buf.push(byte[0]);
                if buf.len() > MAX_FRAME {
                    return Err(FrameError::TooLarge);
                }
                if byte[0] == b'\n' {
                    return Ok(buf);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(FrameError::Eof);
            }
            Err(_) => return Err(FrameError::Io),
        }
    }
}

pub async fn write_json<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    value: &Value,
) -> Result<(), FrameError> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| FrameError::Io)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME {
        return Err(FrameError::TooLarge);
    }
    writer.write_all(&bytes).await.map_err(|_| FrameError::Io)?;
    writer.flush().await.map_err(|_| FrameError::Io)?;
    Ok(())
}

pub fn hello_frame(id: &str, instance_id: &str, revision: u64, config_digest: &str) -> Value {
    json!({
        "id": id,
        "m": "hello",
        "protocol": 2,
        "instance_id": instance_id,
        "revision": revision,
        "config_digest": config_digest,
    })
}

pub fn said_frame(
    id: &str,
    binding_id: &str,
    origin: &str,
    author_id: &str,
    text: &str,
    attachments: &[Attachment],
) -> Value {
    json!({
        "id": id,
        "m": "said",
        "binding_id": binding_id,
        "origin": origin,
        "author_id": author_id,
        "text": text,
        "attachments": attachments.iter().map(|a| json!({"kind": a.kind, "url": a.url})).collect::<Vec<_>>(),
    })
}

pub fn ok_frame(id: &str) -> Value {
    json!({"id": id, "m": "ok"})
}

pub fn err_frame(id: &str, code: &str, detail: Option<&str>) -> Value {
    json!({
        "id": id,
        "m": "err",
        "code": code,
        "detail": detail,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub kind: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub id: String,
    pub binding_id: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Say {
    pub id: String,
    pub binding_id: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub binding_id: String,
    pub activity_id: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireResponse {
    pub id: String,
    pub ok: bool,
    pub seq: Option<Option<i64>>,
    pub code: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreMsg {
    Bind(Bind),
    Say(Say),
    Activity(Activity),
    Response(WireResponse),
    Reverse {
        id: Option<String>,
        m: String,
    },
    Unknown {
        id: Option<String>,
        m: String,
    },
    Invalid {
        id: Option<String>,
        code: &'static str,
        m: String,
    },
}

pub fn parse_frame_bytes(bytes: &[u8]) -> Result<CoreMsg, FrameError> {
    let without_lf = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let text = std::str::from_utf8(without_lf).map_err(|_| FrameError::BadRequest)?;
    let obj = parse_object_no_dup(text.as_bytes()).map_err(|e| match e {
        JsonError::BadRequest => FrameError::BadRequest,
    })?;
    Ok(parse_core_msg(&obj))
}

fn opt_id(obj: &Value) -> Option<String> {
    obj.get("id")
        .and_then(Value::as_str)
        .and_then(|s| parse_request_id(s).ok())
}

fn require_str(obj: &Value, key: &str) -> Result<String, FrameError> {
    match obj.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(FrameError::BadRequest),
    }
}

fn nonempty_str(obj: &Value, key: &str) -> Result<String, FrameError> {
    let s = require_str(obj, key)?;
    if s.is_empty() {
        return Err(FrameError::BadRequest);
    }
    Ok(s)
}

pub fn parse_request_id(raw: &str) -> Result<String, FrameError> {
    let n = raw.len();
    if n == 0 || n > 128 {
        return Err(FrameError::BadRequest);
    }
    Ok(raw.to_string())
}

pub fn parse_uuid(raw: &str) -> Result<String, FrameError> {
    let parsed = uuid::Uuid::parse_str(raw).map_err(|_| FrameError::BadRequest)?;
    let canonical = parsed.to_string();
    if canonical != raw {
        return Err(FrameError::BadRequest);
    }
    Ok(canonical)
}

pub fn parse_digest(raw: &str) -> Result<String, FrameError> {
    if raw.len() != 64 || !raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(FrameError::BadRequest);
    }
    Ok(raw.to_string())
}

pub fn config_bytes(author_id: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"author_id": author_id}))
        .expect("author_id string is JSON-encodable")
}

pub fn config_digest(author_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(config_bytes(author_id));
    hex_lower(&hash)
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

fn parse_core_msg(obj: &Value) -> CoreMsg {
    let m = match require_str(obj, "m") {
        Ok(m) => m,
        Err(_) => {
            return CoreMsg::Invalid {
                id: opt_id(obj),
                code: "bad_request",
                m: String::new(),
            };
        }
    };
    match m.as_str() {
        "bind" => match parse_bind(obj) {
            Ok(b) => CoreMsg::Bind(b),
            Err(_) => CoreMsg::Invalid {
                id: opt_id(obj),
                code: "bad_request",
                m,
            },
        },
        "say" => match parse_say(obj) {
            Ok(s) => CoreMsg::Say(s),
            Err(_) => CoreMsg::Invalid {
                id: opt_id(obj),
                code: "bad_request",
                m,
            },
        },
        "activity" => match parse_activity(obj) {
            Ok(a) => CoreMsg::Activity(a),
            Err(_) => CoreMsg::Invalid {
                id: opt_id(obj),
                code: "bad_request",
                m,
            },
        },
        "ok" | "err" => match parse_response(obj, &m) {
            Ok(resp) => CoreMsg::Response(resp),
            Err(_) => CoreMsg::Invalid {
                id: opt_id(obj),
                code: "response_invalid",
                m,
            },
        },
        "hello" | "said" => CoreMsg::Reverse { id: opt_id(obj), m },
        _ => CoreMsg::Unknown { id: opt_id(obj), m },
    }
}

fn parse_bind(obj: &Value) -> Result<Bind, FrameError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let address = nonempty_str(obj, "address")?;
    Ok(Bind {
        id,
        binding_id,
        address,
    })
}

fn parse_say(obj: &Value) -> Result<Say, FrameError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let payload = obj.get("payload").cloned().ok_or(FrameError::BadRequest)?;
    if !payload.is_object() {
        return Err(FrameError::BadRequest);
    }
    Ok(Say {
        id,
        binding_id,
        payload,
    })
}

fn parse_activity(obj: &Value) -> Result<Activity, FrameError> {
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let activity_id = parse_uuid(&require_str(obj, "activity_id")?)?;
    let state = nonempty_str(obj, "state")?;
    if state != "started" && state != "ended" {
        return Err(FrameError::BadRequest);
    }
    Ok(Activity {
        binding_id,
        activity_id,
        state,
    })
}

fn parse_response(obj: &Value, m: &str) -> Result<WireResponse, FrameError> {
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
            code: None,
            detail: None,
        })
    } else {
        let code = require_str(obj, "code")?;
        let detail = match obj.get("detail") {
            Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            None | Some(_) => return Err(FrameError::BadRequest),
        };
        Ok(WireResponse {
            id,
            ok: false,
            seq: None,
            code: Some(code),
            detail,
        })
    }
}

/// say payload の text。欠落・非 string・空は None（V3: external_rejected、外部 I/O 0）。
pub fn say_text(payload: &Value) -> Option<&str> {
    match payload.get("text") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_bytes_are_compact_author_id() {
        let bytes = config_bytes("owner-1");
        assert_eq!(bytes, br#"{"author_id":"owner-1"}"#);
    }

    #[test]
    fn config_digest_is_sha256_lowerhex() {
        let d = config_digest("owner-1");
        assert_eq!(d.len(), 64);
        assert!(d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(d, config_digest("owner-1"));
        assert_ne!(d, config_digest("owner-2"));
    }

    #[test]
    fn parse_bind_ok() {
        let raw = br#"{"id":"bind:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","m":"bind","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","address":"web-a-c"}"#;
        match parse_frame_bytes(raw).unwrap() {
            CoreMsg::Bind(b) => {
                assert_eq!(b.address, "web-a-c");
                assert_eq!(b.binding_id, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn say_text_ignores_unknown_and_rejects_empty() {
        assert_eq!(say_text(&json!({"text":"hi","extra":1})), Some("hi"));
        assert_eq!(say_text(&json!({"text":""})), None);
        assert_eq!(say_text(&json!({})), None);
    }

    #[test]
    fn duplicate_member_is_bad_request() {
        let raw = br#"{"id":"1","m":"ok","id":"2"}"#;
        assert_eq!(parse_frame_bytes(raw).unwrap_err(), FrameError::BadRequest);
    }
}
