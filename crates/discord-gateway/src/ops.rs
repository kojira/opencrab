//! Discord gateway の DI 能力宣言と invoke handler（設計 §7・§9A / DESIGN-DI-EXTENSION §3/§5）。
//!
//! フェーズ1最小の callback なし能力: `reaction` / `reply` / `resolve`（name UTF-8 昇順）。全 write は
//! 非同期・callback なし（`callback_schema=null`）。core は常時 detach で invoke し、決着を次 turn が
//! 消費する。短縮参照(uN/eN)は core が origin/snowflake へ解決済みで payload に入る（`"format":
//! "short-ref"` 標示 field のみ）。gateway は origin → (channel,message) を導いて REST を叩く（層分離）。
//! 通常発言（say）は能力ではなく素の本文＝[`crate::post`]。system reaction と emoji 詳細スキーマは
//! D17-05/D17-12（profile data）裁定後の後続スライス。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use opencrab_gate_client::{InvokeHandler, InvokeOutcome};

use crate::map::parse_origin;
use crate::transport::{DiscordTransport, TransportOutcome};

/// #915: 1 binding 内の発話 id → Discord 投稿先。発話 id は既存の say delivery_id /
/// reply call_id をそのまま使う。
pub(crate) type DeliveryTargets = Arc<Mutex<HashMap<String, (String, String)>>>;
/// 1 instance が複数 binding を持てるため、binding ごとの対応表を共有する。
pub(crate) type BindingDeliveryTargets = Arc<Mutex<HashMap<String, DeliveryTargets>>>;

pub(crate) async fn targets_for(
    targets: &BindingDeliveryTargets,
    binding_id: &str,
) -> DeliveryTargets {
    targets
        .lock()
        .await
        .entry(binding_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

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
            "会話の e番号のメッセージに絵文字リアクションを付ける。event に e番号、emoji に絵文字。結果は返らず、この呼び出しの後に再度呼び出されることはない（撃ちっぱなし・再開なし）。N 件のリアクションが必要なら、この 1 応答に N 個の reaction 呼び出しを置く。",
            json!({"type": "object", "required": ["event", "emoji"], "properties": {
                "event": ref_prop("対象メッセージの短縮参照（例 e7）"),
                "emoji": str_prop("リアクション絵文字（例 👍）")
            }}),
        ),
        decl(
            "reply",
            "会話の e番号のメッセージに返信する。event に e番号、text に返信本文。結果は返らず、この呼び出しの後に再度呼び出されることはない（撃ちっぱなし・再開なし）。N 件の返信が必要なら、この 1 応答に N 個の reply 呼び出しを置く。",
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
    targets: BindingDeliveryTargets,
}

impl DiscordInvokeHandler {
    pub fn new(transport: Arc<dyn DiscordTransport>) -> Self {
        Self {
            transport,
            targets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn with_targets(
        transport: Arc<dyn DiscordTransport>,
        targets: BindingDeliveryTargets,
    ) -> Self {
        Self { transport, targets }
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

/// reply DI 本文を Discord 上限で分割して配送する。
///
/// 設計 §6.2 は reply の exact API payload / result shape を**要裁定・未確認**とするため、ここは
/// 最小として delivery canon の「長文 chunk」を say と同じ規則（[`crate::post::split_for_discord`]）で
/// 適用する。分割時は **先頭チャンクだけを reply（reference 付き）**、以降は同 channel の通常投稿として
/// **発生順に逐次**送る。fail-fast（既送分はそのまま・以降送らない・自動再送 0）。返す result は say と
/// 同じく**最後のチャンク**の outcome（message id）。※ 分割時に返す eN が「返信そのもの」ではなく
/// 末尾の続きメッセージを指す点は要裁定（統括判断）。
async fn deliver_reply(
    transport: &Arc<dyn DiscordTransport>,
    channel_id: &str,
    message_id: &str,
    text: &str,
) -> TransportOutcome {
    let mut last: Option<TransportOutcome> = None;
    for (i, chunk) in crate::post::split_for_discord(text).into_iter().enumerate() {
        let out = if i == 0 {
            transport
                .reply_message(channel_id, message_id, &chunk)
                .await
        } else {
            transport.create_message(channel_id, &chunk).await
        };
        match out {
            ok @ TransportOutcome::Ok(_) => last = Some(ok),
            // fail-fast: Rejected / Indeterminate はそのまま返し以降のチャンクは送らない。
            other => return other,
        }
    }
    last.unwrap_or(TransportOutcome::Rejected)
}

#[async_trait]
impl InvokeHandler for DiscordInvokeHandler {
    // #900: reply/reaction は発話クラス（ユーザーに見える発言）。resolve は照会クラスなので false。
    fn is_utterance(&self, operation: &str) -> bool {
        matches!(operation, "reply" | "reaction")
    }

    async fn handle(
        &self,
        call_id: &str,
        binding_id: &str,
        operation: &str,
        payload: &Value,
    ) -> InvokeOutcome {
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
                let outcome = deliver_reply(&self.transport, &ch, &msg, text).await;
                if let TransportOutcome::Ok(result) = &outcome {
                    if let Some(message_id) = result.get("message_id").and_then(Value::as_str) {
                        targets_for(&self.targets, binding_id)
                            .await
                            .lock()
                            .await
                            .insert(call_id.to_string(), (ch.clone(), message_id.to_string()));
                    }
                }
                to_invoke_outcome(outcome)
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

    fn dry_run_handler() -> DiscordInvokeHandler {
        DiscordInvokeHandler::new(Arc::new(DryRunTransport))
    }

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
        for name in ["reaction", "reply"] {
            let description = arr
                .iter()
                .find(|d| d["name"] == name)
                .and_then(|d| d["description"].as_str())
                .unwrap();
            // #923: #914（9a6af850）の英文併記形へ戻す（#922 過少化の取り消し）。
            assert!(
                description.contains("結果は返らない（撃ちっぱなし・再開はされない）。"),
                "#923: {name} 説明文に #914 の fire-and-forget 事実文が無い: {description}"
            );
        }
        // #923: reply は #914 英文「put N reply calls in THIS response」を含む。
        let reply_desc = arr
            .iter()
            .find(|d| d["name"] == "reply")
            .and_then(|d| d["description"].as_str())
            .unwrap();
        assert!(
            reply_desc.contains("put N reply calls in THIS response"),
            "#923: reply 説明文に #914 の N 件並置英文が無い: {reply_desc}"
        );
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
        let h = dry_run_handler();
        assert!(matches!(
            h.handle(
                "c1",
                "b",
                "reply",
                &json!({"event": "discord:message:v1:1:2"})
            )
            .await,
            InvokeOutcome::Rejected
        ));
        assert!(matches!(
            h.handle(
                "c2",
                "b",
                "reaction",
                &json!({"event": "discord:message:v1:1:2"})
            )
            .await,
            InvokeOutcome::Rejected
        ));
        // event が origin でない（未解決/不正）→ Rejected。
        assert!(matches!(
            h.handle("c3", "b", "reply", &json!({"event": "e7", "text": "hi"}))
                .await,
            InvokeOutcome::Rejected
        ));
        // 宣言外 operation は fail-closed。
        assert!(matches!(
            h.handle("c4", "b", "delete", &json!({})).await,
            InvokeOutcome::Rejected
        ));
    }

    #[tokio::test]
    async fn valid_reply_reaction_resolve_reach_transport() {
        let h = dry_run_handler();
        assert!(matches!(
            h.handle(
                "c1",
                "b",
                "reply",
                &json!({"event": "discord:message:v1:100:200", "text": "hi"})
            )
            .await,
            InvokeOutcome::Ok(_)
        ));
        assert!(matches!(
            h.handle(
                "c2",
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
                "c3",
                "b",
                "resolve",
                &json!({"ref": "discord:message:v1:100:200"})
            )
            .await,
            InvokeOutcome::Ok(_)
        ));
        // resolve uN（snowflake）。
        assert!(matches!(
            h.handle("c4", "b", "resolve", &json!({"ref": "222"})).await,
            InvokeOutcome::Ok(_)
        ));
        // resolve が origin でも snowflake でもない → Rejected。
        assert!(matches!(
            h.handle("c5", "b", "resolve", &json!({"ref": "garbage"}))
                .await,
            InvokeOutcome::Rejected
        ));
    }

    #[tokio::test]
    async fn only_successful_reply_registers_completed_target() {
        let targets: BindingDeliveryTargets = Arc::new(Mutex::new(HashMap::new()));
        let h = DiscordInvokeHandler::with_targets(Arc::new(DryRunTransport), Arc::clone(&targets));
        assert!(matches!(
            h.handle(
                "reply-call",
                "binding",
                "reply",
                &json!({"event": "discord:message:v1:100:200", "text": "hi"})
            )
            .await,
            InvokeOutcome::Ok(_)
        ));
        assert!(matches!(
            h.handle(
                "reaction-call",
                "binding",
                "reaction",
                &json!({"event": "discord:message:v1:100:200", "emoji": "👍"})
            )
            .await,
            InvokeOutcome::Ok(_)
        ));
        let binding = targets_for(&targets, "binding").await;
        let binding = binding.lock().await;
        assert!(binding.contains_key("reply-call"));
        assert!(!binding.contains_key("reaction-call"));
    }

    #[tokio::test]
    async fn short_reply_is_single_reply_call() {
        use crate::transport::testfake::RecordingTransport;
        let rec = Arc::new(RecordingTransport::default());
        let out = deliver_reply(
            &(rec.clone() as Arc<dyn DiscordTransport>),
            "100",
            "200",
            "hi",
        )
        .await;
        assert_eq!(rec.kinds(), vec!["reply"], "2000字以下は 1 通の reply");
        assert!(matches!(out, TransportOutcome::Ok(_)));
    }

    #[tokio::test]
    async fn long_reply_first_chunk_is_reply_rest_are_plain_and_last_id_returned() {
        use crate::transport::testfake::RecordingTransport;
        let rec = Arc::new(RecordingTransport::default());
        // 2500 字 → 2 チャンク: [0]=reply(reference 付き) / [1]=通常投稿。
        let out = deliver_reply(
            &(rec.clone() as Arc<dyn DiscordTransport>),
            "100",
            "200",
            &"あ".repeat(2500),
        )
        .await;

        assert_eq!(
            rec.kinds(),
            vec!["reply", "say"],
            "先頭のみ reply・以降は通常投稿"
        );
        let bodies = rec.bodies();
        assert_eq!(bodies[0].chars().count(), 2000);
        assert_eq!(bodies[1].chars().count(), 500);
        assert_eq!(bodies.concat(), "あ".repeat(2500), "順序保証・欠落なし");
        // 最後のチャンク（2 回目・連番 1001）の message_id を運ぶ。
        match out {
            TransportOutcome::Ok(v) => {
                assert_eq!(v.get("message_id").and_then(|m| m.as_str()), Some("1001"))
            }
            _ => panic!("expected Ok outcome"),
        }
    }

    #[tokio::test]
    async fn long_reply_fail_fast_stops_on_mid_chunk_error() {
        use crate::transport::testfake::RecordingTransport;
        let rec = Arc::new(RecordingTransport {
            fail_at: Some(1),
            ..Default::default()
        });
        let out = deliver_reply(
            &(rec.clone() as Arc<dyn DiscordTransport>),
            "100",
            "200",
            &"い".repeat(5000),
        )
        .await;
        assert!(matches!(out, TransportOutcome::Rejected));
        assert_eq!(rec.bodies().len(), 2, "途中失敗で以降のチャンクは送らない");
    }

    /// #923: 発話 op（reaction / reply）の description を #914（9a6af850）の英文併記形へ戻す
    /// （#922 の過少化＝#923 の引数破損の過少因子を取り消す・DIRECTION-LOG 509）。
    /// 観測境界は `operation_declarations()` の各 description（ops_projection.rs:158 が verbatim
    /// 投影する実体）。#914 英文断片の存在（#913 断片 assert の復活）＋全文一致で pin する。
    /// 現 tip では #922 の JP-only 文が入っているため **赤**。
    #[test]
    fn utterance_descriptions_match_914_english() {
        let decls = operation_declarations();
        let arr = decls.as_array().unwrap();
        let desc = |name: &str| -> String {
            arr.iter()
                .find(|d| d["name"] == name)
                .and_then(|d| d["description"].as_str())
                .unwrap_or_else(|| panic!("op が無い: {name}"))
                .to_string()
        };

        // #914 英文併記が復活している（#913 断片 assert を戻す・#922 の否定 assert を反転）。
        for name in ["reaction", "reply"] {
            let d = desc(name);
            assert!(
                d.contains("This call returns nothing and you will NOT be invoked again after it"),
                "{name}: #914 の英文が無い（#922 過少化のまま）:\n{d}"
            );
            assert!(
                d.contains("結果は返らない（撃ちっぱなし・再開はされない）"),
                "{name}: #914 の JP 事実文が無い:\n{d}"
            );
        }

        // 全文一致（#914・9a6af850 逐語）。
        assert_eq!(
            desc("reaction"),
            "会話の e番号のメッセージに絵文字リアクションを付ける。event に e番号、emoji に絵文字。結果は返らない（撃ちっぱなし・再開はされない）。複数のリアクションは1回の応答でまとめて呼んでよく、分けて呼び直す必要はない。This call returns nothing and you will NOT be invoked again after it. If you need N reactions, put N reaction calls in THIS response.",
            "reaction 説明文が #914 全文と一致しない"
        );
        assert_eq!(
            desc("reply"),
            "会話の e番号のメッセージに返信する。event に e番号、text に返信本文。結果は返らない（撃ちっぱなし・再開はされない）。複数の返信は1回の応答でまとめて呼んでよく、分けて呼び直す必要はない。This call returns nothing and you will NOT be invoked again after it. If you need N replies, put N reply calls in THIS response.",
            "reply 説明文が #914 全文と一致しない"
        );
    }
}
