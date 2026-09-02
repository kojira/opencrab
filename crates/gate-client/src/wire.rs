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
    /// R2(👀): この started が読み取ったターン発端の origin。core は state="started" にだけ
    /// 載せる（1-said-1-turn）。additive field（DESIGN-EXTGATE-V3 §「認識しない field は無視」）
    /// なので、origin を送らない旧 core / これを見ない旧 gateway とも互換。ended・未載時は None。
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
    if state != "started" && state != "ended" {
        return Err(FrameError::BadRequest);
    }
    // R2(👀): origin は optional。欠落=None（旧 core 互換）。present は nonempty string。
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
/// resume ターン等で送信側が載せる。欠落・非 string・空は None（gateway 側の相関に委ねる）。
pub fn say_reply_target(payload: &Value) -> Option<&str> {
    match payload.get("reply_target") {
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
    fn say_reply_target_reads_optional_origin() {
        assert_eq!(
            say_reply_target(&json!({"text":"hi","reply_target":"nostr:event:v1:default:aa"})),
            Some("nostr:event:v1:default:aa")
        );
        // 欠落・空は None（gateway 側の相関に委ねる）。
        assert_eq!(say_reply_target(&json!({"text":"hi"})), None);
        assert_eq!(
            say_reply_target(&json!({"text":"hi","reply_target":""})),
            None
        );
    }

    #[test]
    fn duplicate_member_is_bad_request() {
        let raw = br#"{"id":"1","m":"ok","id":"2"}"#;
        assert_eq!(parse_frame_bytes(raw).unwrap_err(), FrameError::BadRequest);
    }

    #[test]
    fn parse_invoke_ok() {
        let raw = br#"{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","m":"invoke","binding_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","operation":"reply","context":{"continuation_id":null},"payload":{"event":"e7","text":"hi"}}"#;
        match parse_frame_bytes(raw).unwrap() {
            CoreMsg::Invoke(i) => {
                assert_eq!(i.id, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
                assert_eq!(i.binding_id, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
                assert_eq!(i.operation, "reply");
                assert_eq!(i.continuation_id, None);
                assert_eq!(i.payload, json!({"event":"e7","text":"hi"}));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_invoke_missing_payload_is_bad_request() {
        let raw = br#"{"id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","m":"invoke","binding_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","operation":"reply","context":{"continuation_id":null}}"#;
        match parse_frame_bytes(raw).unwrap() {
            CoreMsg::Invalid { code, .. } => assert_eq!(code, "bad_request"),
            other => panic!("{other:?}"),
        }
    }

    // R2(👀): started は origin を運ぶ。
    #[test]
    fn parse_activity_started_carries_origin() {
        let raw = br#"{"m":"activity","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","state":"started","origin":"omo-1"}"#;
        match parse_frame_bytes(raw).unwrap() {
            CoreMsg::Activity(a) => {
                assert_eq!(a.state, "started");
                assert_eq!(a.origin.as_deref(), Some("omo-1"));
                assert_eq!(a.completed_target, None);
            }
            other => panic!("{other:?}"),
        }
    }

    // R2: origin 欠落（旧 core）は None（後方互換）。additive の未知 field も無視。
    #[test]
    fn parse_activity_without_origin_is_none() {
        let raw = br#"{"m":"activity","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","state":"ended","future_field":42}"#;
        match parse_frame_bytes(raw).unwrap() {
            CoreMsg::Activity(a) => {
                assert_eq!(a.state, "ended");
                assert_eq!(a.origin, None);
                assert_eq!(a.completed_target, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_activity_ended_carries_completed_target() {
        let raw = br#"{"m":"activity","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","activity_id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","state":"ended","completed_target":"cccccccc-cccc-4ccc-8ccc-cccccccccccc"}"#;
        match parse_frame_bytes(raw).unwrap() {
            CoreMsg::Activity(a) => {
                assert_eq!(a.state, "ended");
                assert_eq!(a.origin, None);
                assert_eq!(
                    a.completed_target.as_deref(),
                    Some("cccccccc-cccc-4ccc-8ccc-cccccccccccc")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    // R3(❌): turn_failed は binding_id + origin を運ぶ。error 本文（未知 field）は無視。
    #[test]
    fn parse_turn_failed_ok() {
        let raw = br#"{"m":"turn_failed","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","origin":"boom-1","error":"leaked-should-be-ignored"}"#;
        match parse_frame_bytes(raw).unwrap() {
            CoreMsg::TurnFailed(t) => {
                assert_eq!(t.binding_id, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
                assert_eq!(t.origin, "boom-1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_turn_failed_missing_origin_is_bad_request() {
        let raw = br#"{"m":"turn_failed","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"}"#;
        match parse_frame_bytes(raw).unwrap() {
            CoreMsg::Invalid { code, .. } => assert_eq!(code, "bad_request"),
            other => panic!("{other:?}"),
        }
    }

    // 後方互換: 未知 `m` かつ id 無しの core→gate 通知（turn_failed を知らない旧 gateway 相当）は
    // Unknown に落ち、handle_msg で write 0・keep（close しない）。DESIGN-EXTGATE-V3 §「RUNNING の
    // 未知 m は unknown_message・keep」。これが崩れると外部 DI gateway を壊すので固定する。
    #[test]
    fn unknown_noid_notification_is_ignorable_unknown() {
        let raw = br#"{"m":"turn_failed_v99","binding_id":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","origin":"x"}"#;
        match parse_frame_bytes(raw).unwrap() {
            CoreMsg::Unknown { id, m } => {
                assert_eq!(id, None);
                assert_eq!(m, "turn_failed_v99");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn invoke_ok_frame_carries_result() {
        assert_eq!(
            invoke_ok_frame("call-1", &json!({"ok":true})),
            json!({"id":"call-1","m":"ok","result":{"ok":true}})
        );
        // JSON null は合法な result。
        assert_eq!(
            invoke_ok_frame("call-1", &Value::Null),
            json!({"id":"call-1","m":"ok","result":null})
        );
    }

    #[test]
    fn hello_with_operations_optional() {
        // None は従来の hello（operations field なし＝能力ゼロ）。
        let plain = hello_frame_with_operations("h", "iid", 1, &"a".repeat(64), None);
        assert!(plain.get("operations").is_none());
        // Some は operations を載せる。
        let ops = json!([{"name":"reply"}]);
        let withops = hello_frame_with_operations("h", "iid", 1, &"a".repeat(64), Some(&ops));
        assert_eq!(withops["operations"], ops);
    }
}
