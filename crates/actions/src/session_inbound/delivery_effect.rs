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

/// `EngineResult` を §3.4 の配送 effect に写す。
///
/// core が `NO_REPLY` marker と可視本文を分離済みなので、明示終了かつ本文なしの場合だけ
/// [`DeliveryEffect::NoReply`]、本文があれば [`DeliveryEffect::Text`] にする。
pub fn delivery_effect(
    result: anyhow::Result<EngineResult>,
    _ctx: crate::no_reply::DeliveryContext<'_>,
) -> DeliveryEffect {
    match result {
        Ok(er) if !er.response.is_empty() => DeliveryEffect::Text {
            body: er.response,
            stopped_by_limit: er.stopped_by_limit,
            tool_calls_made: er.tool_calls_made,
            iterations: er.iterations,
        },
        Ok(er) if er.explicit_termination.is_some() => DeliveryEffect::NoReply,
        Ok(_) => DeliveryEffect::Empty,
        Err(e) => DeliveryEffect::Failed {
            error: format!("{e:#}"),
        },
    }
}
