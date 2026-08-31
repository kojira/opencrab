//! Discord gateway の DI 能力宣言と invoke handler（設計 §7・§9A / DESIGN-DI-EXTENSION §3/§5）。
//!
//! フェーズ1最小の callback なし能力: `reaction` / `reply` / `resolve`（name UTF-8 昇順）。全 write は
//! 非同期・callback なし（`callback_schema=null`）。core は常時 detach で invoke し、決着を次 turn が
//! 消費する。短縮参照(uN/eN)は core が origin/snowflake へ解決済みで payload に入る（`"format":
//! "short-ref"` 標示 field のみ）。gateway は origin → (channel,message) を導いて REST を叩く（層分離）。
//! 通常発言（say）は能力ではなく素の本文＝[`crate::post`]。system reaction と emoji 詳細スキーマは
//! D17-05/D17-12（profile data）裁定後の後続スライス。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use opencrab_gate_client::{InvokeHandler, InvokeOutcome};

use crate::map::parse_origin;
use crate::transport::{DiscordTransport, TransportOutcome};

/// hello の `operations` 配列（§7.2・フェーズ1最小・name UTF-8 昇順）。
pub fn operation_declarations() -> Value {
    // 投稿・操作系は sub-engine へ出さない（not_exposed）。イベント参照は conversation_bound
    // （DI-02 の既存 gateway tool 可視性規則に対応・D17-13 で legacy `discord_add_reaction` の
    // Blocked+ConversationBound と同等以上に厳格であることを確認済み）。
    let conv = json!({"sub_engine": "not_exposed", "sharing": "conversation_bound"});
    let decl = |name: &str, desc: &str, input: Value| {
        json!({
            "name": name,
            "description": desc,
            "input_schema": input,
            "output_schema": null,
            "callback_schema": null,
            "class": conv,
        })
    };
    let str_prop = |desc: &str| json!({"type": "string", "description": desc});
    // 短縮参照フィールド（uN/eN）。core はこの標示 field だけを実 ID へ解決する。
    let ref_prop =
        |desc: &str| json!({"type": "string", "description": desc, "format": "short-ref"});
    json!([
        decl(
            "reaction",
            "会話の e番号のメッセージに絵文字リアクションを付ける。event に e番号、emoji に絵文字。",
            json!({"type": "object", "required": ["event", "emoji"], "properties": {
                "event": ref_prop("対象メッセージの短縮参照（例 e7）"),
                "emoji": str_prop("リアクション絵文字（例 👍）")
            }}),
        ),
        decl(
            "reply",
            "会話の e番号のメッセージに返信する。event に e番号、text に返信本文。",
            json!({"type": "object", "required": ["event", "text"], "properties": {
                "event": ref_prop("返信先メッセージの短縮参照（例 e7）"),
                "text": str_prop("返信本文")
            }}),
        ),
        decl(
            "resolve",
            "u番号/e番号の完全な生JSONを取得する（会話で省略された全文の参照）。",
            json!({"type": "object", "required": ["ref"], "properties": {
                "ref": ref_prop("u番号またはe番号の短縮参照（例 u2 / e7）")
            }}),
        ),
    ])
}

/// Discord の invoke handler。短縮参照解決済み payload を Discord REST（transport）へ写す。
pub struct DiscordInvokeHandler {
    transport: Arc<dyn DiscordTransport>,
}

impl DiscordInvokeHandler {
    pub fn new(transport: Arc<dyn DiscordTransport>) -> Self {
        Self { transport }
    }
}

fn str_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn is_decimal(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn to_invoke_outcome(out: TransportOutcome) -> InvokeOutcome {
    match out {
        TransportOutcome::Ok(v) => InvokeOutcome::Ok(v),
        TransportOutcome::Rejected => InvokeOutcome::Rejected,
        TransportOutcome::Indeterminate => InvokeOutcome::Indeterminate,
    }
}

#[async_trait]
impl InvokeHandler for DiscordInvokeHandler {
    async fn handle(&self, _binding_id: &str, operation: &str, payload: &Value) -> InvokeOutcome {
        match operation {
            "reply" => {
                let (Some(event), Some(text)) =
                    (str_field(payload, "event"), str_field(payload, "text"))
                else {
                    return InvokeOutcome::Rejected;
                };
                let Some((ch, msg)) = parse_origin(event) else {
                    return InvokeOutcome::Rejected;
                };
                to_invoke_outcome(self.transport.reply_message(&ch, &msg, text).await)
            }
            "reaction" => {
                let (Some(event), Some(emoji)) =
                    (str_field(payload, "event"), str_field(payload, "emoji"))
                else {
                    return InvokeOutcome::Rejected;
                };
                let Some((ch, msg)) = parse_origin(event) else {
                    return InvokeOutcome::Rejected;
                };
                to_invoke_outcome(self.transport.add_reaction(&ch, &msg, emoji).await)
            }
            "resolve" => {
                let Some(reference) = str_field(payload, "ref") else {
                    return InvokeOutcome::Rejected;
                };
                if let Some((ch, msg)) = parse_origin(reference) {
                    // eN → message の生 JSON。
                    to_invoke_outcome(self.transport.get_message(&ch, &msg).await)
                } else if is_decimal(reference) {
                    // uN → user snowflake の生 JSON。
                    to_invoke_outcome(self.transport.get_user(reference).await)
                } else {
                    InvokeOutcome::Rejected
                }
            }
            // 宣言外は正常な core からは来ない。fail-closed（§5.1）。
            _ => InvokeOutcome::Rejected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::DryRunTransport;

    #[test]
    fn declarations_are_sorted_and_three_callback_free() {
        let decls = operation_declarations();
        let arr = decls.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        let names: Vec<&str> = arr.iter().map(|d| d["name"].as_str().unwrap()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "name UTF-8 昇順（DESIGN-DI §3.1）");
        assert_eq!(names, vec!["reaction", "reply", "resolve"]);
        for d in arr {
            assert!(d["callback_schema"].is_null(), "フェーズ1は callback なし");
            assert_eq!(d["class"]["sub_engine"], "not_exposed");
            assert_eq!(d["class"]["sharing"], "conversation_bound");
        }
    }

    #[test]
    fn short_ref_fields_are_marked() {
        let decls = operation_declarations();
        let arr = decls.as_array().unwrap();
        let reply = arr.iter().find(|d| d["name"] == "reply").unwrap();
        assert_eq!(
            reply["input_schema"]["properties"]["event"]["format"],
            "short-ref"
        );
        // text は生本文なので短縮参照ではない。
        assert!(reply["input_schema"]["properties"]["text"]
            .get("format")
            .is_none());
    }

    #[tokio::test]
    async fn missing_fields_are_rejected_without_io() {
        let h = DiscordInvokeHandler::new(Arc::new(DryRunTransport));
        assert!(matches!(
            h.handle("b", "reply", &json!({"event": "discord:message:v1:1:2"}))
                .await,
            InvokeOutcome::Rejected
        ));
        assert!(matches!(
            h.handle("b", "reaction", &json!({"event": "discord:message:v1:1:2"}))
                .await,
            InvokeOutcome::Rejected
        ));
        // event が origin でない（未解決/不正）→ Rejected。
        assert!(matches!(
            h.handle("b", "reply", &json!({"event": "e7", "text": "hi"}))
                .await,
            InvokeOutcome::Rejected
        ));
        // 宣言外 operation は fail-closed。
        assert!(matches!(
            h.handle("b", "delete", &json!({})).await,
            InvokeOutcome::Rejected
        ));
    }

    #[tokio::test]
    async fn valid_reply_reaction_resolve_reach_transport() {
        let h = DiscordInvokeHandler::new(Arc::new(DryRunTransport));
        assert!(matches!(
            h.handle(
                "b",
                "reply",
                &json!({"event": "discord:message:v1:100:200", "text": "hi"})
            )
            .await,
            InvokeOutcome::Ok(_)
        ));
        assert!(matches!(
            h.handle(
                "b",
                "reaction",
                &json!({"event": "discord:message:v1:100:200", "emoji": "👍"})
            )
            .await,
            InvokeOutcome::Ok(_)
        ));
        // resolve eN（origin）。
        assert!(matches!(
            h.handle(
                "b",
                "resolve",
                &json!({"ref": "discord:message:v1:100:200"})
            )
            .await,
            InvokeOutcome::Ok(_)
        ));
        // resolve uN（snowflake）。
        assert!(matches!(
            h.handle("b", "resolve", &json!({"ref": "222"})).await,
            InvokeOutcome::Ok(_)
        ));
        // resolve が origin でも snowflake でもない → Rejected。
        assert!(matches!(
            h.handle("b", "resolve", &json!({"ref": "garbage"})).await,
            InvokeOutcome::Rejected
        ));
    }
}
