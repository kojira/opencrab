//! Gateway 能力宣言（DI 拡張 §3 / §10.2）。generic declaration の parse・検証・canonical
//! digest。core は operation 名や schema property の platform 意味を解釈しない（§1.7）。
//!
//! 検証失敗は原則 `operation_declaration_invalid`（DI-22）。ただし builtin / 既存 tool との
//! 同名 collision だけは `bad_request`（DI-03）。digest 不一致は呼び出し側（handle_hello）で
//! `operation_declaration_mismatch` を返す。

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{ErrorCode, GateError};

/// 宣言配列上限。frame 全体は既存 1 MiB 上限に従う（protocol.rs）。
const MAX_OPERATIONS: usize = 256;
/// 共通 schema 資源上限（§3.2.3）。上限を迂回する任意再帰 schema は受理しない。
const MAX_SCHEMA_DEPTH: usize = 32;
const MAX_SCHEMA_NODES: usize = 1024;
const MAX_STRING_LEN: usize = 16_384;

/// JSON Schema 2020-12 subset の許可 keyword（DI-03）。これ以外は宣言不正。
// DI-03 の subset + `format`（レビュー要望の generic 参照フィールド標示。core は `format` の値
// 文字列を解釈せず、projection が "short-ref" 標示の field だけを短縮参照解決する。platform 語彙は
// 持たない）。
const ALLOWED_SCHEMA_KEYWORDS: &[&str] = &[
    "type",
    "required",
    "properties",
    "enum",
    "items",
    "description",
    "format",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubEngine {
    NotExposed,
    Blocked,
    Allowed,
}

impl SubEngine {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "not_exposed" => Self::NotExposed,
            "blocked" => Self::Blocked,
            "allowed" => Self::Allowed,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotExposed => "not_exposed",
            Self::Blocked => "blocked",
            Self::Allowed => "allowed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharing {
    AgentBound,
    ConversationBound,
}

impl Sharing {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "agent_bound" => Self::AgentBound,
            "conversation_bound" => Self::ConversationBound,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentBound => "agent_bound",
            Self::ConversationBound => "conversation_bound",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedCaller {
    Owner,
    CoAgent,
    Trusted,
    Guest,
}

impl AllowedCaller {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "owner" => Self::Owner,
            "co_agent" => Self::CoAgent,
            "trusted" => Self::Trusted,
            "guest" => Self::Guest,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::CoAgent => "co_agent",
            Self::Trusted => "trusted",
            Self::Guest => "guest",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationDispatch {
    Inline,
    Background,
    Utterance,
}

impl OperationDispatch {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "inline" => Self::Inline,
            "background" => Self::Background,
            "utterance" => Self::Utterance,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Background => "background",
            Self::Utterance => "utterance",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationEffect {
    ReadOnly,
    StateChange,
    Utterance,
}

impl OperationEffect {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "read_only" => Self::ReadOnly,
            "state_change" => Self::StateChange,
            "utterance" => Self::Utterance,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::StateChange => "state_change",
            Self::Utterance => "utterance",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalDelivery {
    Automatic,
    OperationDriven,
}

impl FinalDelivery {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "automatic" => Self::Automatic,
            "operation_driven" => Self::OperationDriven,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::OperationDriven => "operation_driven",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryGuarantee {
    ExactlyOnce,
    AtMostOnceIndeterminate,
}

impl DeliveryGuarantee {
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "exactly_once" => Self::ExactlyOnce,
            "at_most_once_indeterminate" => Self::AtMostOnceIndeterminate,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExactlyOnce => "exactly_once",
            Self::AtMostOnceIndeterminate => "at_most_once_indeterminate",
        }
    }

    pub fn satisfies(self, required: Self) -> bool {
        matches!(self, Self::ExactlyOnce) || self == required
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationPolicy {
    pub allowed_callers: Vec<AllowedCaller>,
    pub dispatch: OperationDispatch,
    pub sub_engine: SubEngine,
    pub sharing: Sharing,
    pub effect: OperationEffect,
    pub required_delivery_guarantee: Option<DeliveryGuarantee>,
}

/// hello で宣言される 1 能力。immutable snapshot として live entry に保持する。
#[derive(Debug, Clone)]
pub struct GatewayOperationDeclaration {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub callback_schema: Option<Value>,
    pub policy: OperationPolicy,
}

impl GatewayOperationDeclaration {
    /// `callback_schema != null` を callback 能力とする（別 boolean を重複して持たない・§3.1）。
    pub fn is_callback_capable(&self) -> bool {
        self.callback_schema.is_some()
    }

    /// canonical JSON 用に既知 field だけを sorted-key object へ再構成する。unknown field は
    /// 無視されるので digest に混ざらない。serde_json の Map は BTreeMap で key を UTF-8 昇順に
    /// 並べ、`to_vec` は最小セパレータで byte 化する（DI-05・既存 config_b64 正規化と同種）。
    fn to_canonical_value(&self) -> Value {
        let mut authorization = Map::new();
        authorization.insert(
            "allowed_callers".to_string(),
            Value::Array(
                self.policy
                    .allowed_callers
                    .iter()
                    .map(|caller| Value::String(caller.as_str().to_string()))
                    .collect(),
            ),
        );
        let mut obj = Map::new();
        obj.insert("name".to_string(), Value::String(self.name.clone()));
        obj.insert(
            "description".to_string(),
            Value::String(self.description.clone()),
        );
        obj.insert("input_schema".to_string(), self.input_schema.clone());
        obj.insert(
            "output_schema".to_string(),
            self.output_schema.clone().unwrap_or(Value::Null),
        );
        obj.insert(
            "callback_schema".to_string(),
            self.callback_schema.clone().unwrap_or(Value::Null),
        );
        obj.insert("authorization".to_string(), Value::Object(authorization));
        obj.insert(
            "dispatch".to_string(),
            Value::String(self.policy.dispatch.as_str().to_string()),
        );
        obj.insert(
            "sub_engine".to_string(),
            Value::String(self.policy.sub_engine.as_str().to_string()),
        );
        obj.insert(
            "sharing".to_string(),
            Value::String(self.policy.sharing.as_str().to_string()),
        );
        obj.insert(
            "effect".to_string(),
            Value::String(self.policy.effect.as_str().to_string()),
        );
        if let Some(required) = self.policy.required_delivery_guarantee {
            obj.insert(
                "required_delivery_guarantee".to_string(),
                Value::String(required.as_str().to_string()),
            );
        }
        Value::Object(obj)
    }
}

fn invalid() -> GateError {
    GateError::new(ErrorCode::OperationDeclarationInvalid)
}

/// name 文法 `[A-Za-z][A-Za-z0-9_.-]{0,127}`（DI-03）。
fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    if !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// hello の `operations` 配列を検証し、canonical 昇順の宣言 snapshot を返す。
/// `reserved` は builtin / 既存 tool 名との collision 判定（true=予約済み）。collision は
/// `bad_request`、それ以外の宣言不正は `operation_declaration_invalid`。
pub fn validate_operations(
    operations: &Value,
    reserved: &dyn Fn(&str) -> bool,
) -> Result<Vec<GatewayOperationDeclaration>, GateError> {
    let Value::Array(items) = operations else {
        return Err(invalid());
    };
    if items.len() > MAX_OPERATIONS {
        return Err(invalid());
    }
    let mut decls: Vec<GatewayOperationDeclaration> = Vec::with_capacity(items.len());
    for item in items {
        let obj = item.as_object().ok_or_else(invalid)?;
        let decl = parse_declaration(obj)?;
        // builtin / 既存 tool との同名 collision（DI-03 → bad_request）。
        if reserved(&decl.name) {
            return Err(GateError::new(ErrorCode::BadRequest));
        }
        decls.push(decl);
    }
    // 配列は name の UTF-8 byte 列昇順・同名なし（§3.1）。非 sort / 重複は宣言不正。
    for pair in decls.windows(2) {
        if pair[0].name.as_bytes() >= pair[1].name.as_bytes() {
            return Err(invalid());
        }
    }
    Ok(decls)
}

fn parse_declaration(obj: &Map<String, Value>) -> Result<GatewayOperationDeclaration, GateError> {
    let name = match obj.get("name") {
        Some(Value::String(s)) if valid_name(s) => s.clone(),
        _ => return Err(invalid()),
    };
    let description = match obj.get("description") {
        Some(Value::String(s)) if !s.is_empty() && s.len() <= MAX_STRING_LEN => s.clone(),
        _ => return Err(invalid()),
    };
    let input_schema = match obj.get("input_schema") {
        Some(v @ Value::Object(_)) => {
            validate_schema(v)?;
            v.clone()
        }
        _ => return Err(invalid()),
    };
    let output_schema = parse_optional_schema(obj.get("output_schema"))?;
    let callback_schema = parse_optional_schema(obj.get("callback_schema"))?;
    let policy = parse_policy(obj)?;
    Ok(GatewayOperationDeclaration {
        name,
        description,
        input_schema,
        output_schema,
        callback_schema,
        policy,
    })
}

/// non-null は JSON object、null は None。field 欠落も宣言不正（§3.1 は field 必須）。
fn parse_optional_schema(value: Option<&Value>) -> Result<Option<Value>, GateError> {
    match value {
        Some(Value::Null) => Ok(None),
        Some(v @ Value::Object(_)) => {
            validate_schema(v)?;
            Ok(Some(v.clone()))
        }
        _ => Err(invalid()),
    }
}

fn parse_policy(obj: &Map<String, Value>) -> Result<OperationPolicy, GateError> {
    let authorization = obj
        .get("authorization")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let callers = authorization
        .get("allowed_callers")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if callers.is_empty() {
        return Err(invalid());
    }
    let mut allowed_callers = Vec::with_capacity(callers.len());
    let mut previous: Option<&str> = None;
    for raw in callers {
        let raw = raw.as_str().ok_or_else(invalid)?;
        if previous.is_some_and(|prev| prev >= raw) {
            return Err(invalid());
        }
        allowed_callers.push(AllowedCaller::parse(raw).ok_or_else(invalid)?);
        previous = Some(raw);
    }
    let dispatch = obj
        .get("dispatch")
        .and_then(Value::as_str)
        .and_then(OperationDispatch::parse)
        .ok_or_else(invalid)?;
    let sub_engine = obj
        .get("sub_engine")
        .and_then(Value::as_str)
        .and_then(SubEngine::parse)
        .ok_or_else(invalid)?;
    let sharing = obj
        .get("sharing")
        .and_then(Value::as_str)
        .and_then(Sharing::parse)
        .ok_or_else(invalid)?;
    let effect = obj
        .get("effect")
        .and_then(Value::as_str)
        .and_then(OperationEffect::parse)
        .ok_or_else(invalid)?;
    if matches!(dispatch, OperationDispatch::Utterance)
        != matches!(effect, OperationEffect::Utterance)
    {
        return Err(invalid());
    }
    let required_delivery_guarantee = match obj.get("required_delivery_guarantee") {
        None => None,
        Some(Value::String(raw)) => Some(DeliveryGuarantee::parse(raw).ok_or_else(invalid)?),
        Some(_) => return Err(invalid()),
    };
    if required_delivery_guarantee.is_some() && !matches!(dispatch, OperationDispatch::Utterance) {
        return Err(invalid());
    }
    Ok(OperationPolicy {
        allowed_callers,
        dispatch,
        sub_engine,
        sharing,
        effect,
        required_delivery_guarantee,
    })
}

/// JSON Schema 2020-12 subset の検証（DI-03）。許可 keyword のみ・資源上限内・object 構造。
fn validate_schema(schema: &Value) -> Result<(), GateError> {
    let mut nodes = 0usize;
    validate_schema_node(schema, 0, &mut nodes)
}

fn validate_schema_node(node: &Value, depth: usize, nodes: &mut usize) -> Result<(), GateError> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(invalid());
    }
    *nodes += 1;
    if *nodes > MAX_SCHEMA_NODES {
        return Err(invalid());
    }
    // schema node は JSON object（§3.2.2）。
    let obj = node.as_object().ok_or_else(invalid)?;
    for (key, value) in obj {
        if !ALLOWED_SCHEMA_KEYWORDS.contains(&key.as_str()) {
            return Err(invalid());
        }
        match key.as_str() {
            "properties" => {
                let props = value.as_object().ok_or_else(invalid)?;
                for sub in props.values() {
                    validate_schema_node(sub, depth + 1, nodes)?;
                }
            }
            "items" => {
                // items は単一の sub-schema（2020-12 の array items）。
                validate_schema_node(value, depth + 1, nodes)?;
            }
            "required" => {
                let arr = value.as_array().ok_or_else(invalid)?;
                for entry in arr {
                    let s = entry.as_str().ok_or_else(invalid)?;
                    if s.len() > MAX_STRING_LEN {
                        return Err(invalid());
                    }
                    *nodes += 1;
                    if *nodes > MAX_SCHEMA_NODES {
                        return Err(invalid());
                    }
                }
            }
            "enum" => {
                let arr = value.as_array().ok_or_else(invalid)?;
                for entry in arr {
                    check_value_size(entry, nodes)?;
                }
            }
            "type" => match value {
                Value::String(s) => {
                    if s.len() > MAX_STRING_LEN {
                        return Err(invalid());
                    }
                }
                Value::Array(arr) => {
                    for entry in arr {
                        let s = entry.as_str().ok_or_else(invalid)?;
                        if s.len() > MAX_STRING_LEN {
                            return Err(invalid());
                        }
                        *nodes += 1;
                        if *nodes > MAX_SCHEMA_NODES {
                            return Err(invalid());
                        }
                    }
                }
                _ => return Err(invalid()),
            },
            "description" | "format" => {
                let s = value.as_str().ok_or_else(invalid)?;
                if s.len() > MAX_STRING_LEN {
                    return Err(invalid());
                }
            }
            _ => unreachable!("keyword allowlisted above"),
        }
    }
    Ok(())
}

/// enum 値等の任意 JSON の string / node 上限を数える（資源上限迂回の防止）。
fn check_value_size(value: &Value, nodes: &mut usize) -> Result<(), GateError> {
    *nodes += 1;
    if *nodes > MAX_SCHEMA_NODES {
        return Err(invalid());
    }
    match value {
        Value::String(s) => {
            if s.len() > MAX_STRING_LEN {
                return Err(invalid());
            }
        }
        Value::Array(arr) => {
            for entry in arr {
                check_value_size(entry, nodes)?;
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                if k.len() > MAX_STRING_LEN {
                    return Err(invalid());
                }
                check_value_size(v, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// 検証済み宣言配列の canonical JSON の SHA-256 lowerhex（DI-04/05）。宣言が空でも
/// `[]` の digest を返す。宣言順は validate 済みで name 昇順に固定されている。
pub fn declaration_digest(decls: &[GatewayOperationDeclaration]) -> String {
    digest_value(&Value::Array(
        decls.iter().map(|d| d.to_canonical_value()).collect(),
    ))
}

/// Digest-covered protocol-v3 live capability snapshot.
pub fn runtime_declaration_digest(
    decls: &[GatewayOperationDeclaration],
    final_delivery: FinalDelivery,
    delivery_guarantee: DeliveryGuarantee,
) -> String {
    let mut value = Map::new();
    value.insert("version".to_string(), Value::Number(1u64.into()));
    value.insert(
        "final_delivery".to_string(),
        Value::String(final_delivery.as_str().to_string()),
    );
    value.insert(
        "delivery_guarantee".to_string(),
        Value::String(delivery_guarantee.as_str().to_string()),
    );
    value.insert(
        "operations".to_string(),
        Value::Array(decls.iter().map(|d| d.to_canonical_value()).collect()),
    );
    digest_value(&Value::Object(value))
}

pub fn validate_runtime_compatibility(
    decls: &[GatewayOperationDeclaration],
    final_delivery: FinalDelivery,
    delivery_guarantee: DeliveryGuarantee,
) -> Result<(), GateError> {
    if matches!(final_delivery, FinalDelivery::OperationDriven)
        && !decls
            .iter()
            .any(|declaration| matches!(declaration.policy.dispatch, OperationDispatch::Utterance))
    {
        return Err(invalid());
    }
    for declaration in decls {
        if declaration
            .policy
            .required_delivery_guarantee
            .is_some_and(|required| !delivery_guarantee.satisfies(required))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

fn digest_value(value: &Value) -> String {
    // serde_json is deterministic for the canonical maps constructed above.
    let bytes = serde_json::to_vec(value).expect("canonical declaration serialization");
    let hash = Sha256::digest(&bytes);
    let mut out = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in hash.iter() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
#[path = "operations/tests.rs"]
mod tests;
