// core からのライブ通知（state 変更・発話・生成途中・活動）。
//
// `client.rs` から `include!` される。enum を独立ファイルに置くのは 1 ファイル 800 行の
// 上限を守るため（`state_api.rs` が上限に達していた）。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    Message {
        /// core が say に載せた既存 delivery id。gateway は platform message id との対応に使う。
        delivery_id: String,
        text: String,
        /// この say が**特定の inbound イベントへの返信**なら、その said の origin。
        ///
        /// 即時ターン（`occupy_until_turn_ends=true` の said 1 本）でだけ `Some`。bundle
        /// ターン（複数 said・activity started 起源）や、同一ターンに複数の即時 said が
        /// 相乗りした曖昧ケースでは`None`（単一の返信先が無い）。consumerは`Some`を
        /// 対象返信、`None`を「返信先無し」として扱う。返信先を使わないconsumerは無視してよい。
        reply_origin: Option<String>,
        /// D-1083: the partial stream this say completes. `Some` only when core sent `Partial`
        /// events with the same id before this say; the text of those partials is a prefix-like
        /// preview of `text`, which stays the authoritative body.
        stream_id: Option<String>,
    },
    /// D-1083: cumulative released text of a realtime-profile turn's stream, sent before the say
    /// carrying the same `stream_id`. Best effort: dropped silently when the live queue is full.
    /// Consumers that do not preview text ignore it. D-1085: `provisional_id` is set when the
    /// partial comes from a provisional run, which is not a turn until a said promotes it.
    Partial {
        stream_id: String,
        text: String,
        provisional_id: Option<String>,
    },
    /// D-1085: whether the said carrying `provisional_id` reuses that provisional run. Sent
    /// before the turn's say.
    ProvisionalResult {
        provisional_id: String,
        promoted: bool,
    },
    Activity {
        activity_id: String,
        state: String,
        /// #964: 次の LLM request に新しく含める投稿の origin（state="read" のときだけ Some）。
        /// consumerはread+Someを外部側の既読表現へ変換できる。
        origin: Option<String>,
    },
    /// #915: activity ended で core が指定した完了サインの付け先（発話 id）。
    Completed { target: String },
    CompletedNoReply {
        /// 沈黙で終えたターン（say 無し）の発端 origin。即時ターン（`occupy_until_turn_ends`）が
        /// 単独で握った said（`ReplyOrigin::Single`）だけ `Some`。bundle ターンや複数即時 said の
        /// 相乗り（`None`/`Ambiguous`）では単一の発端を決められないので `None`。consumer は `Some` を
        /// 「その発端messageが沈黙で終えた」サインに使い、`None`は無視してよい。
        /// 裁定A（core が ended を say の後に出す）により、返信ターンでは saw_utterance=true のため
        /// このイベントは立たず、真の沈黙ターンだけに立つ。
        reply_origin: Option<String>,
    },
    /// R3(❌): ターン失敗（DeliveryEffect::Failed）。`reply_origin` は発端メッセージの origin。
    /// consumerはこのoriginへ外部側の失敗表現を付けられる。error本文は運ばない。
    TurnFailed { reply_origin: String },
    /// D-1087: ツール実行 1 件の活動（core の `tool_activity` frame）。
    ///
    /// core は宛先も整形も持たない。名前・状態・所要時間・引数/結果（core が既に持っている
    /// 値そのまま）だけを運び、どこへ出すかは consumer が決める。id を持たない
    /// fire-and-forget 通知なので、見ない consumer は無視してよい。
    ToolActivity {
        /// 同じターンの `activity` frame と同じ値。
        activity_id: String,
        call_id: String,
        tool: String,
        /// `started` / `completed` / `failed` / `rejected`。
        status: String,
        depth: u32,
        started_at: String,
        /// 終端（completed/failed/rejected）だけ。
        duration_ms: Option<u64>,
        args: Value,
        /// 終端だけ。
        result: Option<Value>,
        /// failed/rejected だけ。
        error: Option<String>,
    },
    /// D-1087: サブタスク走行 1 件のライフサイクル（core の `subtask_lifecycle` frame）。
    ///
    /// `status` は `started` / `progress` / `completed` / `failed` / `timed_out` / `aborted`。
    /// 分からない項目は `None`（frame が field ごと省く）。
    SubtaskLifecycle {
        run_id: String,
        status: String,
        label: Option<String>,
        task: Option<String>,
        detail: Option<String>,
        duration_ms: Option<u64>,
        result_text: Option<String>,
    },
    Error {
        code: String,
        detail: Option<String>,
    },
}
