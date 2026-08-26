//! frame と message。V3 §3 の完全表以外を作らない。

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;

use crate::error::{ErrorCode, GateError};
use crate::ids::{parse_digest, parse_request_id, parse_uuid};
use crate::json::parse_object_no_dup;

/// LF 込み上限。
pub const MAX_FRAME: usize = 1_048_576;

#[derive(Debug)]
pub enum FrameError {
    TooLarge,
    Eof,
    Io,
}

pub async fn read_frame<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> Result<Vec<u8>, FrameError> {
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

pub async fn write_json(
    writer: &tokio::sync::Mutex<OwnedWriteHalf>,
    value: &Value,
) -> Result<(), GateError> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| GateError::store())?;
    bytes.push(b'\n');
    let mut w = writer.lock().await;
    w.write_all(&bytes)
        .await
        .map_err(|_| GateError::new(ErrorCode::Disconnect))?;
    w.flush()
        .await
        .map_err(|_| GateError::new(ErrorCode::Disconnect))?;
    Ok(())
}

pub fn ok_frame(id: &str) -> Value {
    json!({"id": id, "m": "ok"})
}

pub fn ok_said_frame(id: &str, seq: Option<i64>) -> Value {
    json!({"id": id, "m": "ok", "seq": seq})
}

pub fn err_frame(id: &str, code: ErrorCode, detail: Option<&str>) -> Value {
    json!({
        "id": id,
        "m": "err",
        "code": code.as_str(),
        "detail": detail,
    })
}

pub fn bind_frame(binding_id: &str, address: &str) -> Value {
    json!({
        "id": crate::ids::bind_request_id(binding_id),
        "m": "bind",
        "binding_id": binding_id,
        "address": address,
    })
}

pub fn say_frame(delivery_id: &str, binding_id: &str, body: &str) -> Value {
    json!({
        "id": delivery_id,
        "m": "say",
        "binding_id": binding_id,
        "payload": {"text": body},
    })
}

pub fn activity_frame(binding_id: &str, activity_id: &str, state: &str) -> Value {
    json!({
        "m": "activity",
        "binding_id": binding_id,
        "activity_id": activity_id,
        "state": state,
    })
}

#[derive(Debug, Clone)]
pub struct Hello {
    pub id: String,
    pub protocol: u64,
    pub instance_id: String,
    pub revision: u64,
    pub config_digest: String,
}

#[derive(Debug, Clone)]
pub struct Said {
    pub id: String,
    pub binding_id: String,
    pub origin: String,
    pub author_id: String,
    pub text: String,
    pub attachments: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WireResponse {
    pub id: String,
    pub ok: bool,
    pub seq: Option<Option<i64>>,
    pub code: Option<ErrorCode>,
    pub detail: Option<String>,
}

#[derive(Debug)]
pub enum InboundMsg {
    Hello(Hello),
    Said(Said),
    Response(WireResponse),
    Reverse { id: Option<String>, m: String },
    Unknown { id: Option<String>, m: String },
    Invalid {
        id: Option<String>,
        code: ErrorCode,
        m: String,
    },
}

pub fn parse_frame_bytes(bytes: &[u8]) -> Result<InboundMsg, GateError> {
    let without_lf = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let text = std::str::from_utf8(without_lf).map_err(|_| GateError::new(ErrorCode::BadRequest))?;
    let obj = parse_object_no_dup(text.as_bytes())?;
    parse_inbound(&obj)
}

fn opt_id(obj: &Value) -> Option<String> {
    obj.get("id")
        .and_then(Value::as_str)
        .and_then(|s| parse_request_id(s).ok())
}

fn require_str(obj: &Value, key: &str) -> Result<String, GateError> {
    match obj.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

fn nonempty_str(obj: &Value, key: &str) -> Result<String, GateError> {
    let s = require_str(obj, key)?;
    if s.is_empty() {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    Ok(s)
}

fn require_u64(obj: &Value, key: &str) -> Result<u64, GateError> {
    match obj.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| GateError::new(ErrorCode::BadRequest)),
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

pub fn parse_inbound(obj: &Value) -> Result<InboundMsg, GateError> {
    let m = require_str(obj, "m")?;
    match m.as_str() {
        "hello" => match parse_hello(obj) {
            Ok(h) => Ok(InboundMsg::Hello(h)),
            Err(e) => Ok(InboundMsg::Invalid {
                id: opt_id(obj),
                code: e.code,
                m,
            }),
        },
        "said" => match parse_said(obj) {
            Ok(s) => Ok(InboundMsg::Said(s)),
            Err(e) => Ok(InboundMsg::Invalid {
                id: opt_id(obj),
                code: e.code,
                m,
            }),
        },
        "ok" | "err" => match parse_response(obj, &m) {
            Ok(resp) => Ok(InboundMsg::Response(resp)),
            Err(_) => Ok(InboundMsg::Invalid {
                id: opt_id(obj),
                code: ErrorCode::ResponseInvalid,
                m,
            }),
        },
        "bind" | "say" | "activity" => Ok(InboundMsg::Reverse {
            id: opt_id(obj),
            m,
        }),
        _ => Ok(InboundMsg::Unknown {
            id: opt_id(obj),
            m,
        }),
    }
}

fn parse_hello(obj: &Value) -> Result<Hello, GateError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let protocol = require_u64(obj, "protocol")?;
    let instance_id = parse_uuid(&require_str(obj, "instance_id")?)?;
    let revision = require_u64(obj, "revision")?;
    if revision == 0 {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    let config_digest = parse_digest(&require_str(obj, "config_digest")?)?;
    Ok(Hello {
        id,
        protocol,
        instance_id,
        revision,
        config_digest,
    })
}

fn parse_said(obj: &Value) -> Result<Said, GateError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let origin = nonempty_str(obj, "origin")?;
    let author_id = nonempty_str(obj, "author_id")?;
    let text = require_str(obj, "text")?;
    let attachments = parse_attachments(obj.get("attachments"))?;
    if text.is_empty() && attachments.is_empty() {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    Ok(Said {
        id,
        binding_id,
        origin,
        author_id,
        text,
        attachments,
    })
}

fn parse_attachments(value: Option<&Value>) -> Result<Vec<String>, GateError> {
    let Some(Value::Array(items)) = value else {
        return Err(GateError::new(ErrorCode::BadRequest));
    };
    let mut urls = Vec::with_capacity(items.len());
    for item in items {
        let obj = item
            .as_object()
            .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
        let kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
        if kind != "image" {
            return Err(GateError::new(ErrorCode::BadRequest));
        }
        let url = obj
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
        if !is_absolute_https(url) {
            return Err(GateError::new(ErrorCode::BadRequest));
        }
        urls.push(url.to_string());
    }
    Ok(urls)
}

fn is_absolute_https(url: &str) -> bool {
    url.starts_with("https://") && url.len() > "https://".len()
}

fn parse_response(obj: &Value, m: &str) -> Result<WireResponse, GateError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    if m == "ok" {
        let seq = match obj.get("seq") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::Number(n)) => {
                let v = n
                    .as_i64()
                    .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
                if v <= 0 {
                    return Err(GateError::new(ErrorCode::BadRequest));
                }
                Some(Some(v))
            }
            Some(_) => return Err(GateError::new(ErrorCode::BadRequest)),
        };
        Ok(WireResponse {
            id,
            ok: true,
            seq,
            code: None,
            detail: None,
        })
    } else {
        let code_raw = require_str(obj, "code")?;
        let code = ErrorCode::parse(&code_raw).ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
        let detail = match obj.get("detail") {
            Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            None | Some(_) => return Err(GateError::new(ErrorCode::BadRequest)),
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
