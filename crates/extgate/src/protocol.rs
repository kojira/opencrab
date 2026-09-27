//! frame と message。V3 §3 の完全表以外を作らない。

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;

use crate::error::{ErrorCode, GateError};
use crate::ids::{parse_digest, parse_request_id, parse_uuid};
use crate::json::parse_object_no_dup;
use crate::operations::GatewayOperationDeclaration;

/// LF 込み上限。
pub const MAX_FRAME: usize = 1_048_576;

#[derive(Debug)]
pub enum FrameError {
    TooLarge,
    Eof,
    Io,
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

pub fn command_ok_frame(id: &str, result: &Value) -> Value {
    json!({"id": id, "m": "ok", "result": result})
}

pub fn command_err_frame(id: &str, code: &str, message: &str) -> Value {
    json!({"id": id, "m": "err", "code": code, "message": message})
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

/// say フレーム。`reply_target` を渡すと payload に載る（gateway が返信先を導けない
/// resume ターン等で、発端イベントの origin を明示する）。省略時は gateway 側の相関
/// （直前に送った said の origin）に委ねる。
pub fn say_frame(
    delivery_id: &str,
    binding_id: &str,
    body: &str,
    reply_target: Option<&str>,
) -> Value {
    let mut payload = json!({ "text": body });
    if let Some(target) = reply_target {
        payload["reply_target"] = json!(target);
    }
    json!({
        "id": delivery_id,
        "m": "say",
        "binding_id": binding_id,
        "payload": payload,
    })
}

/// invoke フレーム（DI 拡張 §5.1）。`id=call_id`。第一段は callback 無しなので
/// `context.continuation_id` は常に null。payload は opaque JSON-value。
pub fn invoke_frame(
    call_id: &str,
    binding_id: &str,
    declaration_digest: &str,
    declaration: &GatewayOperationDeclaration,
    continuation_id: Option<&str>,
    payload: &Value,
) -> Value {
    json!({
        "id": call_id,
        "m": "invoke",
        "invocation_protocol": 1,
        "binding_id": binding_id,
        "declaration_digest": declaration_digest,
        "operation": declaration.name,
        "dispatch": declaration.policy.dispatch.as_str(),
        "effect": declaration.policy.effect.as_str(),
        "context": {"continuation_id": continuation_id},
        "payload": payload,
    })
}

pub fn activity_frame(
    binding_id: &str,
    activity_id: &str,
    state: &str,
    origin: Option<&str>,
    completed_target: Option<&str>,
) -> Value {
    let mut frame = json!({
        "m": "activity",
        "binding_id": binding_id,
        "activity_id": activity_id,
        "state": state,
    });
    // #964: read が示す「次の LLM request に新しく含めた投稿」の origin を additive に載せる。
    // 旧 gateway は認識しない field を無視するので互換（DESIGN-EXTGATE-V3 §「認識しない field は無視」）。
    if let Some(origin) = origin {
        frame["origin"] = json!(origin);
    }
    // #915: ended が完了サインを付ける発話 id を additive に指定する。発話 id は say の
    // delivery_id / reply の call_id を流用し、無いときは field 自体を省略する。
    if let Some(target) = completed_target {
        frame["completed_target"] = json!(target);
    }
    frame
}

/// New coreのsuccessful ended outcome。`silent_origins`はemptyでも必ず載せ、
/// field欠落を旧core互換fallbackと区別できるようにする。
pub fn ended_activity_frame(
    binding_id: &str,
    activity_id: &str,
    completed_target: Option<&str>,
    silent_origins: &[String],
) -> Value {
    let mut frame = activity_frame(binding_id, activity_id, "ended", None, completed_target);
    frame["silent_origins"] = json!(silent_origins);
    frame
}

/// R3(❌): ターン失敗通知フレーム。id を持たない fire-and-forget 通知（activity と同型）。
/// 未知フレームを ignore する準拠 gateway は write 0・keep で素通しする。error 本文は載せない。
pub fn turn_failed_frame(binding_id: &str, origin: &str) -> Value {
    json!({
        "m": "turn_failed",
        "binding_id": binding_id,
        "origin": origin,
    })
}

#[derive(Debug, Clone)]
pub struct Hello {
    pub id: String,
    pub protocol: u64,
    pub instance_id: String,
    pub revision: u64,
    pub config_digest: String,
    /// Versioned dynamic-operation declaration envelope.
    pub operation_protocol: u64,
    pub final_delivery: String,
    pub operations: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaidAttachment {
    /// Existing compatibility shape.
    ImageUrl(String),
    /// File downloaded by a co-located external gateway. `local_path` is
    /// relative to the configured attachment inbox root.
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
pub enum SaidCaller {
    Owner,
    Agent,
    CoAgent { agent_id: String },
    TrustedUser,
}

#[derive(Debug, Clone)]
pub struct CreateBinding {
    pub id: String,
    pub binding_id: String,
    pub address: String,
    pub session_theme: String,
}

/// One pre-hello declarative generic projection. Platform configuration never appears here.
#[derive(Debug, Clone)]
pub struct Provision {
    pub id: String,
    pub instance_id: String,
    pub kind_id: String,
    pub subject_id: i64,
    pub subject_grant: Option<String>,
    pub adopt_existing: bool,
    pub enabled: bool,
    pub config_b64: String,
    pub bindings: Vec<ProvisionBinding>,
}

#[derive(Debug, Clone)]
pub struct ProvisionBinding {
    pub binding_id: String,
    pub address: String,
}

#[derive(Debug, Clone)]
pub struct Command {
    pub id: String,
    pub binding_id: String,
    pub caller: SaidCaller,
    pub name: String,
    pub args: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone)]
pub struct Said {
    pub id: String,
    pub binding_id: String,
    pub origin: String,
    pub author_id: String,
    /// Untrusted, display-only label supplied by the gateway.
    pub author_label: Option<String>,
    /// Caller classification asserted by the operator-installed gateway.
    pub caller: SaidCaller,
    /// Whether this record starts a model turn. False supports provider-neutral batch staging.
    pub start_turn: bool,
    /// Optional gateway-provided system context. Core treats it as opaque text.
    pub system_context: Option<String>,
    /// Optional external reply reference, persisted without interpretation.
    pub reply_target: Option<String>,
    /// Restrict live inbound folding to this speaker without naming a platform.
    pub only_speaker: bool,
    pub text: String,
    pub attachments: Vec<SaidAttachment>,
}

impl Said {
    pub fn image_urls(&self) -> Vec<String> {
        self.attachments
            .iter()
            .filter_map(|attachment| match attachment {
                SaidAttachment::ImageUrl(url) => Some(url.clone()),
                SaidAttachment::LocalFile { .. } => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct WireResponse {
    pub id: String,
    pub ok: bool,
    pub seq: Option<Option<i64>>,
    /// invoke `ok` の `result`。None=field 欠落、Some(Value::Null)=合法な JSON null（§10.2）。
    pub result: Option<Value>,
    pub code: Option<ErrorCode>,
    pub detail: Option<String>,
}

#[derive(Debug)]
pub enum InboundMsg {
    Hello(Hello),
    Provision(Provision),
    CreateBinding(CreateBinding),
    Command(Command),
    Said(Said),
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
        code: ErrorCode,
        m: String,
    },
}

pub fn parse_frame_bytes(bytes: &[u8]) -> Result<InboundMsg, GateError> {
    let without_lf = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let text =
        std::str::from_utf8(without_lf).map_err(|_| GateError::new(ErrorCode::BadRequest))?;
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
        "provision" => match parse_provision(obj) {
            Ok(request) => Ok(InboundMsg::Provision(request)),
            Err(e) => Ok(InboundMsg::Invalid {
                id: opt_id(obj),
                code: e.code,
                m,
            }),
        },
        "create_binding" => match parse_create_binding(obj) {
            Ok(request) => Ok(InboundMsg::CreateBinding(request)),
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
        "command" => match parse_command(obj) {
            Ok(command) => Ok(InboundMsg::Command(command)),
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
        "bind" | "say" | "activity" => Ok(InboundMsg::Reverse { id: opt_id(obj), m }),
        _ => Ok(InboundMsg::Unknown { id: opt_id(obj), m }),
    }
}

fn parse_provision(obj: &Value) -> Result<Provision, GateError> {
    const FIELDS: &[&str] = &[
        "m",
        "id",
        "instance_id",
        "kind_id",
        "subject_id",
        "subject_grant",
        "adopt_existing",
        "enabled",
        "config_b64",
        "bindings",
    ];
    let source = obj
        .as_object()
        .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    if source.keys().any(|key| !FIELDS.contains(&key.as_str())) {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let instance_id = parse_uuid(&require_str(obj, "instance_id")?)?;
    let kind_id = nonempty_str(obj, "kind_id")?;
    let subject_id = match obj.get("subject_id") {
        Some(Value::Number(value)) => value.as_i64().filter(|value| *value > 0),
        _ => None,
    }
    .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    let subject_grant = match obj.get("subject_grant") {
        None => None,
        Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
        _ => return Err(GateError::new(ErrorCode::BadRequest)),
    };
    let adopt_existing = match obj.get("adopt_existing") {
        None => false,
        Some(Value::Bool(value)) => *value,
        _ => return Err(GateError::new(ErrorCode::BadRequest)),
    };
    let enabled = match obj.get("enabled") {
        Some(Value::Bool(value)) => *value,
        _ => return Err(GateError::new(ErrorCode::BadRequest)),
    };
    let config_b64 = nonempty_str(obj, "config_b64")?;
    crate::ids::decode_config_b64(&config_b64)?;
    let values = obj
        .get("bindings")
        .and_then(Value::as_array)
        .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    let mut binding_ids = std::collections::HashSet::new();
    let mut addresses = std::collections::HashSet::new();
    let mut bindings = Vec::with_capacity(values.len());
    for value in values {
        let binding = value
            .as_object()
            .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
        if binding.len() != 2
            || !binding.contains_key("binding_id")
            || !binding.contains_key("address")
        {
            return Err(GateError::new(ErrorCode::BadRequest));
        }
        let binding_id = binding
            .get("binding_id")
            .and_then(Value::as_str)
            .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
        let binding_id = parse_uuid(binding_id)?;
        let address = binding
            .get("address")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?
            .to_string();
        if !binding_ids.insert(binding_id.clone()) || !addresses.insert(address.clone()) {
            return Err(GateError::new(ErrorCode::BadRequest));
        }
        bindings.push(ProvisionBinding {
            binding_id,
            address,
        });
    }
    Ok(Provision {
        id,
        instance_id,
        kind_id,
        subject_id,
        subject_grant,
        adopt_existing,
        enabled,
        config_b64,
        bindings,
    })
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
    let operation_protocol = require_u64(obj, "operation_protocol")?;
    let final_delivery = require_str(obj, "final_delivery")?;
    let operations = obj
        .get("operations")
        .cloned()
        .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    Ok(Hello {
        id,
        protocol,
        instance_id,
        revision,
        config_digest,
        operation_protocol,
        final_delivery,
        operations,
    })
}

fn parse_create_binding(obj: &Value) -> Result<CreateBinding, GateError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let address = nonempty_str(obj, "address")?;
    let session_theme = nonempty_str(obj, "session_theme")?;
    Ok(CreateBinding {
        id,
        binding_id,
        address,
        session_theme,
    })
}

fn parse_command(obj: &Value) -> Result<Command, GateError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let caller_value = obj
        .get("caller")
        .filter(|value| value.is_object())
        .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    let caller = parse_said_caller(Some(caller_value))?;
    let name = nonempty_str(obj, "name")?;
    let args = obj
        .get("args")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    Ok(Command {
        id,
        binding_id,
        caller,
        name,
        args,
    })
}

fn parse_said(obj: &Value) -> Result<Said, GateError> {
    let id = parse_request_id(&require_str(obj, "id")?)?;
    let binding_id = parse_uuid(&require_str(obj, "binding_id")?)?;
    let origin = nonempty_str(obj, "origin")?;
    let author_id = nonempty_str(obj, "author_id")?;
    let author_label = match obj.get("author_label") {
        None | Some(Value::Null) => None,
        Some(Value::String(label))
            if !label.trim().is_empty()
                && label.chars().count() <= 100
                && !label.chars().any(char::is_control) =>
        {
            Some(label.clone())
        }
        _ => return Err(GateError::new(ErrorCode::BadRequest)),
    };
    let caller = parse_said_caller(obj.get("caller"))?;
    let start_turn = match obj.get("start_turn") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(value)) => *value,
        _ => return Err(GateError::new(ErrorCode::BadRequest)),
    };
    let system_context = optional_bounded_text(obj.get("system_context"), 16_384)?;
    let reply_target = optional_bounded_text(obj.get("reply_target"), 4_096)?;
    let only_speaker = match obj.get("live_inbound_scope") {
        None | Some(Value::Null) => false,
        Some(Value::String(scope)) if scope == "all" => false,
        Some(Value::String(scope)) if scope == "speaker" => true,
        _ => return Err(GateError::new(ErrorCode::BadRequest)),
    };
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
        author_label,
        caller,
        start_turn,
        system_context,
        reply_target,
        only_speaker,
        text,
        attachments,
    })
}

fn parse_said_caller(value: Option<&Value>) -> Result<SaidCaller, GateError> {
    let Some(Value::Object(obj)) = value else {
        return Ok(SaidCaller::Agent);
    };
    match obj.get("role").and_then(Value::as_str) {
        Some("owner") => Ok(SaidCaller::Owner),
        Some("agent") => Ok(SaidCaller::Agent),
        Some("trusted_user") => Ok(SaidCaller::TrustedUser),
        Some("co_agent") => {
            let agent_id = nonempty_map_str(obj, "agent_id")?;
            Ok(SaidCaller::CoAgent { agent_id })
        }
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

fn optional_bounded_text(
    value: Option<&Value>,
    max_bytes: usize,
) -> Result<Option<String>, GateError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.is_empty() && text.len() <= max_bytes => {
            Ok(Some(text.clone()))
        }
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

fn parse_attachments(value: Option<&Value>) -> Result<Vec<SaidAttachment>, GateError> {
    let Some(Value::Array(items)) = value else {
        return Err(GateError::new(ErrorCode::BadRequest));
    };
    items.iter().map(parse_attachment).collect()
}

fn parse_attachment(item: &Value) -> Result<SaidAttachment, GateError> {
    let obj = item
        .as_object()
        .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    match obj.get("kind").and_then(Value::as_str) {
        Some("image") => {
            let url = obj
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
            if !is_absolute_https(url) {
                return Err(GateError::new(ErrorCode::BadRequest));
            }
            Ok(SaidAttachment::ImageUrl(url.to_string()))
        }
        Some("file") => parse_local_attachment(obj),
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
}

fn parse_local_attachment(
    obj: &serde_json::Map<String, Value>,
) -> Result<SaidAttachment, GateError> {
    let id = parse_uuid(&nonempty_map_str(obj, "id")?)?;
    let name = nonempty_map_str(obj, "name")?;
    if name.chars().count() > 255
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    let media_type = match obj.get("media_type") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if !value.is_empty() && value.is_ascii() => Some(value.clone()),
        _ => return Err(GateError::new(ErrorCode::BadRequest)),
    };
    let size = obj
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
    let sha256 = parse_digest(&nonempty_map_str(obj, "sha256")?)?;
    let local_path = nonempty_map_str(obj, "local_path")?;
    let path = std::path::Path::new(&local_path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(GateError::new(ErrorCode::BadRequest));
    }
    Ok(SaidAttachment::LocalFile {
        id,
        name,
        media_type,
        size,
        sha256,
        local_path,
    })
}

fn nonempty_map_str(obj: &serde_json::Map<String, Value>, key: &str) -> Result<String, GateError> {
    match obj.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(GateError::new(ErrorCode::BadRequest)),
    }
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
        // invoke `ok` の result。field 欠落=None、present は Value（null 含む）。
        let result = obj.get("result").cloned();
        Ok(WireResponse {
            id,
            ok: true,
            seq,
            result,
            code: None,
            detail: None,
        })
    } else {
        let code_raw = require_str(obj, "code")?;
        let code =
            ErrorCode::parse(&code_raw).ok_or_else(|| GateError::new(ErrorCode::BadRequest))?;
        let detail = match obj.get("detail") {
            Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            None | Some(_) => return Err(GateError::new(ErrorCode::BadRequest)),
        };
        Ok(WireResponse {
            id,
            ok: false,
            seq: None,
            result: None,
            code: Some(code),
            detail,
        })
    }
}

#[cfg(test)]
#[path = "protocol/activity_tests.rs"]
mod activity_tests;
