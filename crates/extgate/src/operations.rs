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
pub struct OperationClass {
    pub sub_engine: SubEngine,
    pub sharing: Sharing,
    /// 発話クラスの**宣言による自己申告**（DESIGN-RESUME-SETTLE §3.3.1 C2・R3 (a)）。
    /// 第一段の分類は core 既知名（`is_known_utterance_op`）が担い、この field は将来の外部
    /// DI gateway 拡張に備えた additive な併設で、`None`（未宣言）は後方互換で無視される。
    /// **digest には含めない**（`to_canonical_value` 参照）——既存 gateway の宣言 digest を
    /// 変えず、分類は routing の便宜であって wire 契約の一部ではないため（sub_engine 等の
    /// セキュリティ属性のみ digest 契約に残す）。
    pub utterance: Option<bool>,
}

/// hello で宣言される 1 能力。immutable snapshot として live entry に保持する。
#[derive(Debug, Clone)]
pub struct GatewayOperationDeclaration {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub callback_schema: Option<Value>,
    pub class: OperationClass,
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
        let mut class = Map::new();
        class.insert(
            "sub_engine".to_string(),
            Value::String(self.class.sub_engine.as_str().to_string()),
        );
        class.insert(
            "sharing".to_string(),
            Value::String(self.class.sharing.as_str().to_string()),
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
        obj.insert("class".to_string(), Value::Object(class));
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
    let class = parse_class(obj.get("class"))?;
    Ok(GatewayOperationDeclaration {
        name,
        description,
        input_schema,
        output_schema,
        callback_schema,
        class,
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

fn parse_class(value: Option<&Value>) -> Result<OperationClass, GateError> {
    let obj = value.and_then(Value::as_object).ok_or_else(invalid)?;
    // class は sub_engine / sharing の 2 field だけ（余剰 member は無視、値は generic enum）。
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
    // additive・後方互換: `utterance` は任意。bool 以外・欠落は None（core 既知名へフォール
    // バック）。宣言不正にはしない（未知/欠落 field は無視の後方互換・§3.3.1 C2）。
    let utterance = obj.get("utterance").and_then(Value::as_bool);
    Ok(OperationClass {
        sub_engine,
        sharing,
        utterance,
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
    let array = Value::Array(decls.iter().map(|d| d.to_canonical_value()).collect());
    // serde_json は BTreeMap key 順 + 最小セパレータ。既存 config_b64 と同じ正規化系。
    let bytes = serde_json::to_vec(&array).expect("canonical declaration serialization");
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
mod tests {
    use super::*;
    use serde_json::json;

    fn no_reserved(_: &str) -> bool {
        false
    }

    fn decl(name: &str) -> Value {
        json!({
            "name": name,
            "description": "d",
            "input_schema": {"type": "object"},
            "output_schema": null,
            "callback_schema": null,
            "class": {"sub_engine": "allowed", "sharing": "conversation_bound"}
        })
    }

    #[test]
    fn empty_array_is_valid_zero_tools() {
        let decls = validate_operations(&json!([]), &no_reserved).unwrap();
        assert!(decls.is_empty());
        // 空配列の digest も安定。
        assert_eq!(declaration_digest(&decls).len(), 64);
    }

    #[test]
    fn sorted_names_ok_unsorted_rejected() {
        let ok = validate_operations(&json!([decl("a"), decl("b")]), &no_reserved).unwrap();
        assert_eq!(ok.len(), 2);
        let err = validate_operations(&json!([decl("b"), decl("a")]), &no_reserved).unwrap_err();
        assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
    }

    #[test]
    fn duplicate_name_rejected() {
        let err = validate_operations(&json!([decl("a"), decl("a")]), &no_reserved).unwrap_err();
        assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
    }

    #[test]
    fn reserved_name_is_bad_request() {
        let reserved = |n: &str| n == "reply";
        let err = validate_operations(&json!([decl("reply")]), &reserved).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadRequest);
    }

    #[test]
    fn bad_name_grammar_rejected() {
        let err = validate_operations(&json!([decl("1bad")]), &no_reserved).unwrap_err();
        assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
        let err = validate_operations(&json!([decl("has space")]), &no_reserved).unwrap_err();
        assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
    }

    #[test]
    fn empty_description_rejected() {
        let mut d = decl("a");
        d["description"] = json!("");
        let err = validate_operations(&json!([d]), &no_reserved).unwrap_err();
        assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
    }

    #[test]
    fn schema_must_be_object() {
        let mut d = decl("a");
        d["input_schema"] = json!("not-object");
        let err = validate_operations(&json!([d]), &no_reserved).unwrap_err();
        assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
    }

    #[test]
    fn schema_disallowed_keyword_rejected() {
        let mut d = decl("a");
        d["input_schema"] = json!({"type": "object", "additionalProperties": false});
        let err = validate_operations(&json!([d]), &no_reserved).unwrap_err();
        assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
    }

    #[test]
    fn nested_properties_recurse_and_allow() {
        let mut d = decl("a");
        d["input_schema"] = json!({
            "type": "object",
            "required": ["event", "text"],
            "properties": {
                "event": {"type": "string", "description": "e番号"},
                "text": {"type": "string"}
            }
        });
        let ok = validate_operations(&json!([d]), &no_reserved).unwrap();
        assert_eq!(ok.len(), 1);
    }

    #[test]
    fn class_unknown_enum_rejected() {
        let mut d = decl("a");
        d["class"] = json!({"sub_engine": "nope", "sharing": "agent_bound"});
        let err = validate_operations(&json!([d]), &no_reserved).unwrap_err();
        assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
    }

    #[test]
    fn missing_field_rejected() {
        let mut d = decl("a");
        d.as_object_mut().unwrap().remove("output_schema");
        let err = validate_operations(&json!([d]), &no_reserved).unwrap_err();
        assert_eq!(err.code, ErrorCode::OperationDeclarationInvalid);
    }

    #[test]
    fn digest_stable_across_member_order() {
        // schema object の member 順が違っても digest は同値（DI-05 golden 性質）。
        let mut a = decl("x");
        a["input_schema"] = json!({"type": "object", "description": "z"});
        let mut b = decl("x");
        b["input_schema"] = json!({"description": "z", "type": "object"});
        let da = validate_operations(&json!([a]), &no_reserved).unwrap();
        let db = validate_operations(&json!([b]), &no_reserved).unwrap();
        assert_eq!(declaration_digest(&da), declaration_digest(&db));
    }

    #[test]
    fn digest_changes_with_declaration() {
        let one = validate_operations(&json!([decl("a")]), &no_reserved).unwrap();
        let two = validate_operations(&json!([decl("a"), decl("b")]), &no_reserved).unwrap();
        assert_ne!(declaration_digest(&one), declaration_digest(&two));
    }

    #[test]
    fn callback_capable_flag_follows_schema() {
        let mut d = decl("a");
        d["callback_schema"] = json!({"type": "object"});
        let decls = validate_operations(&json!([d]), &no_reserved).unwrap();
        assert!(decls[0].is_callback_capable());
        let plain = validate_operations(&json!([decl("a")]), &no_reserved).unwrap();
        assert!(!plain[0].is_callback_capable());
    }
}
