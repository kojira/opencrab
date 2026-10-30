//! V3 §3 の frame と message。core crate の DTO は使わない。

use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::json::{parse_object_no_dup, JsonError};

mod activity;
mod hello_env;
pub use hello_env::hello_frame_with_capabilities_and_env;
mod partial;
mod provisional;
mod response;
mod turn_profile;
pub use activity::{SubtaskLifecycle, ToolActivity};
pub use partial::{say_stream_id, Partial};
pub use provisional::{
    interrupt_frame, provisional_cancel_frame, provisional_frame, ProvisionalRequest,
    ProvisionalResult,
};
pub use turn_profile::TurnModelProfile;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalDelivery {
    Automatic,
    OperationDriven,
}

impl FinalDelivery {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::OperationDriven => "operation_driven",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    pub final_delivery: FinalDelivery,
}

impl Default for RuntimeCapabilities {
    fn default() -> Self {
        Self {
            final_delivery: FinalDelivery::Automatic,
        }
    }
}

/// D-1099: `hello` names the instance and the agent it speaks for. Core finds or creates the
/// instance; nothing else is registered.
pub fn hello_frame(id: &str, instance_id: &str, agent_id: &str) -> Value {
    hello_frame_with_capabilities(
        id,
        instance_id,
        agent_id,
        None,
        RuntimeCapabilities::default(),
    )
}

pub fn hello_frame_with_operations(
    id: &str,
    instance_id: &str,
    agent_id: &str,
    operations: Option<&Value>,
) -> Value {
    hello_frame_with_capabilities(
        id,
        instance_id,
        agent_id,
        operations,
        RuntimeCapabilities::default(),
    )
}

pub fn hello_frame_with_capabilities(
    id: &str,
    instance_id: &str,
    agent_id: &str,
    operations: Option<&Value>,
    capabilities: RuntimeCapabilities,
) -> Value {
    hello_env::hello_frame_with_capabilities_and_env(
        id,
        instance_id,
        agent_id,
        operations,
        capabilities,
        &std::collections::BTreeMap::new(),
    )
}

/// Rebuild the canonical runtime snapshot digest from the hello data retained by this client.
/// Only declaration fields understood by protocol v1 participate, matching extgate validation.
pub(crate) fn runtime_declaration_digest_from_value(
    operations: Option<&Value>,
    capabilities: RuntimeCapabilities,
) -> Option<String> {
    use sha2::{Digest, Sha256};

    let operations = operations
        .cloned()
        .unwrap_or_else(|| json!([]))
        .as_array()
        .cloned()?;
    let mut canonical_operations = Vec::with_capacity(operations.len());
    for operation in operations {
        let source = operation.as_object()?;
        let authorization_source = source.get("authorization")?.as_object()?;
        let mut authorization = serde_json::Map::new();
        authorization.insert(
            "allowed_callers".to_string(),
            authorization_source.get("allowed_callers")?.clone(),
        );
        let mut canonical = serde_json::Map::new();
        for field in [
            "name",
            "description",
            "input_schema",
            "output_schema",
            "callback_schema",
        ] {
            canonical.insert(field.to_string(), source.get(field)?.clone());
        }
        canonical.insert("authorization".to_string(), Value::Object(authorization));
        for field in ["dispatch", "sub_engine", "sharing", "effect"] {
            canonical.insert(field.to_string(), source.get(field)?.clone());
        }
        canonical_operations.push(Value::Object(canonical));
    }
    let snapshot = json!({
        "version": 1,
        "final_delivery": capabilities.final_delivery.as_str(),
        "operations": canonical_operations,
    });
    let bytes = serde_json::to_vec(&snapshot).ok()?;
    Some(hex_lower(&Sha256::digest(bytes)))
}

/// D-1099: `open` — ask core for the session of a key before its first said. Answered with
/// `ok{session_id}`. Idempotent.
pub fn open_frame(id: &str, session_key: &str, title: Option<&str>) -> Value {
    let mut frame = json!({"id": id, "m": "open", "session": session_key});
    if let Some(title) = title {
        frame["title"] = json!(title);
    }
    frame
}

pub fn said_frame(
    id: &str,
    session_key: &str,
    origin: &str,
    author_id: &str,
    text: &str,
    attachments: &[Attachment],
) -> Value {
    said_frame_with_author_label(id, session_key, origin, author_id, None, text, attachments)
}

pub fn said_frame_with_author_label(
    id: &str,
    session_key: &str,
    origin: &str,
    author_id: &str,
    author_label: Option<&str>,
    text: &str,
    attachments: &[Attachment],
) -> Value {
    said_frame_with_context(
        id,
        session_key,
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
    /// D-1099: the opaque key other messages use to point at this one (their `reply_target`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub live_inbound_scope: LiveInboundScope,
    /// D-1081: kind of turn this said starts. Sent only when not `Default`.
    #[serde(skip_serializing_if = "TurnModelProfile::is_default")]
    pub model_profile: TurnModelProfile,
    /// D-1085: text the agent already spoke to the user before this reply (at most 64 chars).
    /// Core records it as the agent's own line right after this said; it is not delivered back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_prelude: Option<String>,
    /// D-1085: the provisional run this said may promote. Core answers with a
    /// `provisional_result` before the turn's say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provisional_id: Option<String>,
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
    session_key: &str,
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
        "session": session_key,
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
        if let Some(reference) = &context.reference {
            frame["ref"] = json!(reference);
        }
        frame["live_inbound_scope"] =
            serde_json::to_value(context.live_inbound_scope).expect("scope serializes");
        if !context.model_profile.is_default() {
            frame["model_profile"] =
                serde_json::to_value(context.model_profile).expect("profile serializes");
        }
        if let Some(prelude) = &context.agent_prelude {
            frame["agent_prelude"] = json!(prelude);
        }
        if let Some(provisional_id) = &context.provisional_id {
            frame["provisional_id"] = json!(provisional_id);
        }
    }
    frame
}

pub fn ok_frame(id: &str) -> Value {
    json!({"id": id, "m": "ok"})
}

pub fn command_frame(
    id: &str,
    session_key: &str,
    caller: &SaidCaller,
    name: &str,
    args: &Value,
) -> Value {
    json!({
        "m": "command",
        "id": id,
        "session": session_key,
        "caller": caller,
        "name": name,
        "args": args,
    })
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
pub struct Say {
    pub id: String,
    /// The gateway's session key (D-1099).
    pub session_key: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub session_key: String,
    pub activity_id: String,
    pub state: String,
    /// #964: 次の LLM request に新しく含める投稿の origin。core は state="read" にだけ載せる。
    /// additive field（DESIGN-EXTGATE-V3 §「認識しない field は無視」）なので、origin を送らない
    /// 旧 core / これを見ない旧 gateway とも互換。started / ended / 未載時は None。
    pub origin: Option<String>,
    /// #915: ended で完了サインを付ける発話 id（say delivery_id / reply call_id）。
    /// additive field なので旧 core の欠落は None。
    pub completed_target: Option<String>,
    /// New coreがendedへ必ず載せるauthoritative silent outcome。`None`は旧coreの
    /// field欠落、`Some(vec![])`は確認済みの沈黙なしで、意味を区別する。
    pub silent_origins: Option<Vec<String>>,
}

/// R3(❌): core→gate のターン失敗通知（DeliveryEffect::Failed）。id を持たない fire-and-forget
/// 通知（activity と同型）なので、未知フレームを ignore する準拠 gateway（外部 DI gateway 含む）は
/// write 0・keep で素通しする。error 本文は載せない（多エージェント相互反応ループ防止・#668）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailed {
    pub session_key: String,
    pub origin: String,
}

/// core→gate invoke（DI 拡張 §5.1）。`id`=call_id。第一段は callback 無しなので
/// `context.continuation_id` は常に null。payload は opaque JSON-value。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invoke {
    pub id: String,
    pub session_key: String,
    pub declaration_digest: String,
    pub operation: String,
    pub dispatch: String,
    pub effect: String,
    pub continuation_id: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireResponse {
    pub id: String,
    pub ok: bool,
    pub seq: Option<Option<i64>>,
    pub result: Option<Value>,
    /// D-1099: the core session a said's or `open`'s key resolved to.
    pub session_id: Option<String>,
    pub code: Option<String>,
    pub detail: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreMsg {
    Say(Say),
    Activity(Activity),
    ToolActivity(activity::ToolActivity),
    SubtaskLifecycle(activity::SubtaskLifecycle),
    TurnFailed(TurnFailed),
    Partial(Partial),
    ProvisionalResult(ProvisionalResult),
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
        "tool_activity" => match activity::parse_tool_activity(obj) {
            Ok(a) => CoreMsg::ToolActivity(a),
            Err(_) => CoreMsg::Invalid {
                id: opt_id(obj),
                code: "bad_request",
                m,
            },
        },
        "subtask_lifecycle" => match activity::parse_subtask_lifecycle(obj) {
            Ok(l) => CoreMsg::SubtaskLifecycle(l),
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
        "partial" => match partial::parse_partial(obj) {
            Ok(p) => CoreMsg::Partial(p),
            Err(_) => CoreMsg::Invalid {
                id: opt_id(obj),
                code: "bad_request",
                m,
            },
        },
        "provisional_result" => match provisional::parse_provisional_result(obj) {
            Ok(r) => CoreMsg::ProvisionalResult(r),
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
        "ok" | "err" => match response::parse_response(obj, &m) {
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

fn parse_say(obj: &Value) -> Result<Say, FrameError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let session_key = nonempty_str(obj, "session")?;
    let payload = obj.get("payload").cloned().ok_or(FrameError::BadRequest)?;
    if !payload.is_object() {
        return Err(FrameError::BadRequest);
    }
    say_reply_target(&payload)?;
    Ok(Say {
        id,
        session_key,
        payload,
    })
}

fn parse_invoke(obj: &Value) -> Result<Invoke, FrameError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let session_key = nonempty_str(obj, "session")?;
    let declaration_digest = parse_digest(&require_str(obj, "declaration_digest")?)?;
    let operation = nonempty_str(obj, "operation")?;
    if obj.get("invocation_protocol").and_then(Value::as_u64) != Some(1) {
        return Err(FrameError::BadRequest);
    }
    let dispatch = nonempty_str(obj, "dispatch")?;
    if !matches!(dispatch.as_str(), "inline" | "background" | "utterance") {
        return Err(FrameError::BadRequest);
    }
    let effect = nonempty_str(obj, "effect")?;
    if !matches!(effect.as_str(), "read_only" | "state_change" | "utterance") {
        return Err(FrameError::BadRequest);
    }
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
        session_key,
        declaration_digest,
        operation,
        dispatch,
        effect,
        continuation_id,
        payload,
    })
}

fn parse_activity(obj: &Value) -> Result<Activity, FrameError> {
    let session_key = nonempty_str(obj, "session")?;
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
    let silent_origins = match obj.get("silent_origins") {
        None => None,
        Some(Value::Array(values)) if state == "ended" => {
            let mut origins = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    Value::String(origin) if !origin.is_empty() => origins.push(origin.clone()),
                    _ => return Err(FrameError::BadRequest),
                }
            }
            Some(origins)
        }
        Some(_) => return Err(FrameError::BadRequest),
    };
    Ok(Activity {
        session_key,
        activity_id,
        state,
        origin,
        completed_target,
        silent_origins,
    })
}

fn parse_turn_failed(obj: &Value) -> Result<TurnFailed, FrameError> {
    let session_key = nonempty_str(obj, "session")?;
    let origin = nonempty_str(obj, "origin")?;
    Ok(TurnFailed {
        session_key,
        origin,
    })
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
