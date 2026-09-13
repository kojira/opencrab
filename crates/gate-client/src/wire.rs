//! V3 §3 の frame と message。core crate の DTO は使わない。

use serde::Serialize;
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

/// 能力宣言つき hello（DI 拡張 §3.1）。`operations` が None なら従来の hello（能力ゼロ）。
pub fn hello_frame_with_operations(
    id: &str,
    instance_id: &str,
    revision: u64,
    config_digest: &str,
    operations: Option<&Value>,
) -> Value {
    let mut frame = hello_frame(id, instance_id, revision, config_digest);
    if let Some(ops) = operations {
        frame["operations"] = ops.clone();
    }
    frame
}

pub fn create_binding_frame(
    id: &str,
    binding_id: &str,
    address: &str,
    session_theme: &str,
) -> Value {
    json!({
        "id": id,
        "m": "create_binding",
        "binding_id": binding_id,
        "address": address,
        "session_theme": session_theme,
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
    said_frame_with_author_label(id, binding_id, origin, author_id, None, text, attachments)
}

pub fn said_frame_with_author_label(
    id: &str,
    binding_id: &str,
    origin: &str,
    author_id: &str,
    author_label: Option<&str>,
    text: &str,
    attachments: &[Attachment],
) -> Value {
    said_frame_with_context(
        id,
        binding_id,
        origin,
        author_id,
        author_label,
        None,
        text,
        attachments,
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SaidContext {
    pub caller: SaidCaller,
    pub start_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_target: Option<String>,
    pub live_inbound_scope: LiveInboundScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum SaidCaller {
    Owner,
    Agent,
    CoAgent { agent_id: String },
    TrustedUser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveInboundScope {
    All,
    Speaker,
}

#[allow(clippy::too_many_arguments)]
pub fn said_frame_with_context(
    id: &str,
    binding_id: &str,
    origin: &str,
    author_id: &str,
    author_label: Option<&str>,
    context: Option<&SaidContext>,
    text: &str,
    attachments: &[Attachment],
) -> Value {
    let mut frame = json!({
        "id": id,
        "m": "said",
        "binding_id": binding_id,
        "origin": origin,
        "author_id": author_id,
        "text": text,
        "attachments": attachments,
    });
    if let Some(label) = author_label {
        frame["author_label"] = json!(label);
    }
    if let Some(context) = context {
        frame["caller"] = serde_json::to_value(&context.caller).expect("caller serializes");
        frame["start_turn"] = json!(context.start_turn);
        if let Some(system) = &context.system_context {
            frame["system_context"] = json!(system);
        }
        if let Some(target) = &context.reply_target {
            frame["reply_target"] = json!(target);
        }
        frame["live_inbound_scope"] =
            serde_json::to_value(context.live_inbound_scope).expect("scope serializes");
    }
    frame
}

pub fn ok_frame(id: &str) -> Value {
    json!({"id": id, "m": "ok"})
}

/// invoke 成功応答（DI 拡張 §5.1）。`result` は opaque JSON-value（null 含む）。
pub fn invoke_ok_frame(id: &str, result: &Value) -> Value {
    json!({"id": id, "m": "ok", "result": result})
}

pub fn err_frame(id: &str, code: &str, detail: Option<&str>) -> Value {
    json!({
        "id": id,
        "m": "err",
        "code": code,
        "detail": detail,
    })
}

/// Provider-neutral inbound attachment carried by a Said frame.
///
/// `ImageUrl` is the existing compatibility shape. New external gateways use
/// `LocalFile`; the source URL stays inside the gateway process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind")]
pub enum Attachment {
    #[serde(rename = "image")]
    ImageUrl { url: String },
    #[serde(rename = "file")]
    LocalFile {
        id: String,
        name: String,
        media_type: Option<String>,
        size: u64,
        sha256: String,
        local_path: String,
    },
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
    /// #964: 次の LLM request に新しく含める投稿の origin。core は state="read" にだけ載せる。
    /// additive field（DESIGN-EXTGATE-V3 §「認識しない field は無視」）なので、origin を送らない
    /// 旧 core / これを見ない旧 gateway とも互換。started / ended / 未載時は None。
    pub origin: Option<String>,
    /// #915: ended で完了サインを付ける発話 id（say delivery_id / reply call_id）。
    /// additive field なので旧 core の欠落は None。
    pub completed_target: Option<String>,
}

/// R3(❌): core→gate のターン失敗通知（DeliveryEffect::Failed）。id を持たない fire-and-forget
/// 通知（activity と同型）なので、未知フレームを ignore する準拠 gateway（外部 DI gateway 含む）は
/// write 0・keep で素通しする。error 本文は載せない（多エージェント相互反応ループ防止・#668）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailed {
    pub binding_id: String,
    pub origin: String,
}

/// core→gate invoke（DI 拡張 §5.1）。`id`=call_id。第一段は callback 無しなので
/// `context.continuation_id` は常に null。payload は opaque JSON-value。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invoke {
    pub id: String,
    pub binding_id: String,
    pub operation: String,
    pub continuation_id: Option<String>,
    pub payload: Value,
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
    TurnFailed(TurnFailed),
    Invoke(Invoke),
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
        "turn_failed" => match parse_turn_failed(obj) {
            Ok(t) => CoreMsg::TurnFailed(t),
            Err(_) => CoreMsg::Invalid {
                id: opt_id(obj),
                code: "bad_request",
                m,
            },
        },
        "invoke" => match parse_invoke(obj) {
            Ok(i) => CoreMsg::Invoke(i),
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
    say_reply_target(&payload)?;
    Ok(Say {
        id,
        binding_id,
        payload,
    })
}

fn parse_invoke(obj: &Value) -> Result<Invoke, FrameError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let operation = nonempty_str(obj, "operation")?;
    let payload = obj.get("payload").cloned().ok_or(FrameError::BadRequest)?;
    // context.continuation_id は第一段では常に null（callback 無し）。
    let continuation_id = match obj.get("context") {
        None | Some(Value::Null) => None,
        Some(Value::Object(ctx)) => match ctx.get("continuation_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(parse_uuid(s)?),
            Some(_) => return Err(FrameError::BadRequest),
        },
        Some(_) => return Err(FrameError::BadRequest),
    };
    Ok(Invoke {
        id,
        binding_id,
        operation,
        continuation_id,
        payload,
    })
}

fn parse_activity(obj: &Value) -> Result<Activity, FrameError> {
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let activity_id = parse_uuid(&require_str(obj, "activity_id")?)?;
    let state = nonempty_str(obj, "state")?;
    // #930: read state を additive に受理（started/ended に加える）。origin つきで 👀 を付ける。
    // 未知 state は従来どおり拒否（既知集合のみ通す）。
    if state != "started" && state != "ended" && state != "read" {
        return Err(FrameError::BadRequest);
    }
    // R2(👀)/#930(read): origin は optional。欠落=None（旧 core 互換）。present は nonempty string。
    let origin = match obj.get("origin") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(_) => return Err(FrameError::BadRequest),
    };
    let completed_target = match obj.get("completed_target") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(_) => return Err(FrameError::BadRequest),
    };
    Ok(Activity {
        binding_id,
        activity_id,
        state,
        origin,
        completed_target,
    })
}

fn parse_turn_failed(obj: &Value) -> Result<TurnFailed, FrameError> {
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let origin = nonempty_str(obj, "origin")?;
    Ok(TurnFailed { binding_id, origin })
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

/// say payload の明示 `reply_target`（発端イベントの origin）。gateway が返信先を導けない
/// resume ターン等で送信側が載せる。欠落時だけ Ok(None)、present-but-invalidはBadRequest。
pub fn say_reply_target(payload: &Value) -> Result<Option<&str>, FrameError> {
    match payload.get("reply_target") {
        None => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.as_str())),
        Some(_) => Err(FrameError::BadRequest),
    }
}

#[cfg(test)]
#[path = "wire/tests.rs"]
mod tests;
