use opencrab_core::EngineResult;

/// 配送 effect（§3.4）。ゲートはこれを既存の送信・リアクションで出す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryEffect {
    Text {
        body: String,
        stopped_by_limit: bool,
        tool_calls_made: usize,
        iterations: usize,
    },
    NoReply,
    Empty,
    Failed {
        error: String,
    },
}

/// `EngineResult` を §3.4 の配送 effect に写す。NO_REPLY 終端解釈（第一柱）はここに集約。
///
/// R4: `NO_REPLY` は**出現＝終端**。最初の `NO_REPLY` で発言を打ち切り、前段が空なら
/// [`DeliveryEffect::NoReply`]、非空ならその前段のみを [`DeliveryEffect::Text`] にする。
/// `NO_REPLY` の後に非空テキストが続いていた場合は `ctx` を相関キーに破棄ログ（§3.1.1）を残す。
pub fn delivery_effect(
    result: anyhow::Result<EngineResult>,
    ctx: crate::no_reply::DeliveryContext<'_>,
) -> DeliveryEffect {
    match result {
        Ok(er) if !er.response.is_empty() => {
            // NO_REPLY 終端（第一柱）→ CONTINUE 末尾剥がし（#890 §11）を 1 経路で確定。
            match crate::continue_marker::visible_speech_after_markers(&er.response, ctx) {
                None => DeliveryEffect::NoReply,
                Some(body) => DeliveryEffect::Text {
                    body,
                    stopped_by_limit: er.stopped_by_limit,
                    tool_calls_made: er.tool_calls_made,
                    iterations: er.iterations,
                },
            }
        }
        Ok(_) => DeliveryEffect::Empty,
        Err(e) => DeliveryEffect::Failed {
            error: format!("{e:#}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn er(response: &str) -> EngineResult {
        EngineResult {
            response: response.into(),
            iterations: 1,
            tool_calls_made: 2,
            stopped_by_limit: false,
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: false,
            xml_fallback_parses: 0,
        }
    }

    #[test]
    fn delivery_effect_maps_engine_result() {
        let ctx = crate::no_reply::DeliveryContext::default();
        assert_eq!(
            delivery_effect(Ok(er("hello")), ctx),
            DeliveryEffect::Text {
                body: "hello".into(),
                stopped_by_limit: false,
                tool_calls_made: 2,
                iterations: 1,
            }
        );
        assert_eq!(
            delivery_effect(Ok(er("NO_REPLY")), ctx),
            DeliveryEffect::NoReply
        );
        let empty = EngineResult {
            response: String::new(),
            iterations: 0,
            tool_calls_made: 0,
            stopped_by_limit: false,
            last_posting_utterance_id: None,
            last_generation_had_continuation_speech: false,
            xml_fallback_parses: 0,
        };
        assert_eq!(delivery_effect(Ok(empty), ctx), DeliveryEffect::Empty);
        match delivery_effect(Err(anyhow::anyhow!("boom")), ctx) {
            DeliveryEffect::Failed { error } => assert!(error.contains("boom"), "{error}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// A1（第一柱・終端化）: 本文 → NO_REPLY → ゴミ の応答は前段本文で確定し、
    /// 配送 body に NO_REPLY もゴミも含めない。
    #[test]
    fn delivery_effect_terminates_at_no_reply() {
        let ctx = crate::no_reply::DeliveryContext::default();
        match delivery_effect(Ok(er("本文だけ話す NO_REPLY これはゴミ")), ctx) {
            DeliveryEffect::Text { body, .. } => {
                assert_eq!(body, "本文だけ話す");
                assert!(!body.contains("NO_REPLY"), "body に NO_REPLY 混入: {body}");
                assert!(!body.contains("ゴミ"), "body に破棄テキスト混入: {body}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
        // 前段なし + 後続あり → NoReply（発言はしない）。
        assert_eq!(
            delivery_effect(Ok(er("NO_REPLY 続くゴミ")), ctx),
            DeliveryEffect::NoReply
        );
    }

    /// #890 §11: 末尾 CONTINUE マーカーは配送 body に残さない（継続判定は engine が済ませ、
    /// ここは表示保護）。途中出現は本文のまま残す。
    #[test]
    fn delivery_effect_strips_tail_continue_marker() {
        let ctx = crate::no_reply::DeliveryContext::default();
        match delivery_effect(Ok(er("確認して返すね⚡\nCONTINUE")), ctx) {
            DeliveryEffect::Text { body, .. } => {
                assert_eq!(body, "確認して返すね⚡");
                assert!(!body.contains("CONTINUE"), "body に CONTINUE 混入: {body}");
            }
            other => panic!("expected Text, got {other:?}"),
        }
        // 途中出現は剥がさない。
        match delivery_effect(Ok(er("まず CONTINUE を確認します")), ctx) {
            DeliveryEffect::Text { body, .. } => {
                assert_eq!(body, "まず CONTINUE を確認します");
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }
}
