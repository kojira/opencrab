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
