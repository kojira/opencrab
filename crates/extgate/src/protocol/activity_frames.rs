//! D-1087: 走行の活動を gateway へ流す 2 つの frame（`tool_activity` / `subtask_lifecycle`）。
//!
//! どちらも id を持たない fire-and-forget 通知（`partial` / `turn_failed` と同型）なので、
//! 未知フレームを無視する準拠 gateway は write 0・keep で素通しする。core は宛先も整形も
//! 持たず、値だけを載せる。

use serde_json::{json, Value};

/// ツール実行 1 件の活動通知（D-1087）。id を持たない fire-and-forget 通知（`partial` /
/// `turn_failed` と同型）。未知フレームを ignore する準拠 gateway は write 0・keep で
/// 素通しするので、活動を見ない gateway はこのフレームで一切変わらない。
///
/// `activity_id` は同じターンの `activity` フレームと同じ値。`args` / `result` は core が
/// 既に持っている値をそのまま載せ、要約・整形・宛先は gateway が決める。終端状態だけが
/// `duration_ms` / `result` / `error` を持つ。
/// frame の field と 1:1 に対応するので、引数をまとめない（形がそのまま契約）。
#[allow(clippy::too_many_arguments)]
pub fn tool_activity_frame(
    binding_id: &str,
    activity_id: &str,
    call_id: &str,
    tool: &str,
    status: &str,
    depth: u32,
    started_at: &str,
    duration_ms: Option<u64>,
    args: &Value,
    result: Option<&Value>,
    error: Option<&str>,
) -> Value {
    let mut frame = json!({
        "m": "tool_activity",
        "binding_id": binding_id,
        "activity_id": activity_id,
        "call_id": call_id,
        "tool": tool,
        "status": status,
        "depth": depth,
        "started_at": started_at,
        "args": args,
    });
    if let Some(duration_ms) = duration_ms {
        frame["duration_ms"] = json!(duration_ms);
    }
    if let Some(result) = result {
        frame["result"] = result.clone();
    }
    if let Some(error) = error {
        frame["error"] = json!(error);
    }
    frame
}

/// サブタスク走行 1 件のライフサイクル通知（D-1087）。`tool_activity` と同じく id を
/// 持たない fire-and-forget。`run_id` は subtask の ID、`status` は
/// `started` / `progress` / `completed` / `failed` / `timed_out` / `aborted`。
/// `sub_session_id` は走行自身のセッション ID（run の相関キー。宛先ではない）。
///
/// 分からない項目は field ごと省く（旧 gateway は認識しない field を無視する）。
#[allow(clippy::too_many_arguments)]
pub fn subtask_lifecycle_frame(
    binding_id: &str,
    run_id: &str,
    sub_session_id: &str,
    status: &str,
    label: &str,
    task: &str,
    detail: Option<&str>,
    duration_ms: Option<u64>,
    result_text: Option<&str>,
) -> Value {
    let mut frame = json!({
        "m": "subtask_lifecycle",
        "binding_id": binding_id,
        "run_id": run_id,
        "status": status,
    });
    if !sub_session_id.is_empty() {
        frame["sub_session_id"] = json!(sub_session_id);
    }
    if !label.is_empty() {
        frame["label"] = json!(label);
    }
    if !task.is_empty() {
        frame["task"] = json!(task);
    }
    if let Some(detail) = detail {
        frame["detail"] = json!(detail);
    }
    if let Some(duration_ms) = duration_ms {
        frame["duration_ms"] = json!(duration_ms);
    }
    if let Some(result_text) = result_text {
        frame["result_text"] = json!(result_text);
    }
    frame
}
