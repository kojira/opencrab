//! D-1087: core→gate `tool_activity` / `subtask_lifecycle` frames.
//!
//! どちらも id を持たない fire-and-forget 通知（`partial` / `turn_failed` と同型）なので、
//! gateway は応答を返さない。core は宛先も整形も持たず、値だけを載せる。知らない consumer は
//! このフレームを無視してよく、その場合 core の挙動も記録も変わらない。

use serde_json::Value;

use super::{nonempty_str, FrameError};

/// ツール実行 1 件の活動通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolActivity {
    pub session_key: String,
    /// 同じターンの `activity` frame と同じ値。
    pub activity_id: String,
    pub call_id: String,
    /// 入れ子の走行（`depth > 0`）で起きたツール実行だけが持つ run の相関キー。
    /// `subtask_lifecycle` の同じ名前の field と突き合わせる。frame が省くことがある。
    pub sub_session_id: Option<String>,
    pub tool: String,
    /// `started` / `completed` / `failed` / `rejected`。
    pub status: String,
    pub depth: u32,
    pub started_at: String,
    /// 終端（completed/failed/rejected）だけ。
    pub duration_ms: Option<u64>,
    /// core が持っている値そのまま（整形は consumer の責務）。
    pub args: Value,
    /// 終端だけ。
    pub result: Option<Value>,
    /// failed/rejected だけ。
    pub error: Option<String>,
}

/// サブタスク走行 1 件のライフサイクル通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtaskLifecycle {
    pub session_key: String,
    /// subtask の ID。
    pub run_id: String,
    /// 走行自身のセッション ID（run の相関キー。宛先ではない）。
    pub sub_session_id: Option<String>,
    /// `started` / `progress` / `completed` / `failed` / `timed_out` / `aborted`。
    pub status: String,
    pub label: Option<String>,
    /// started だけ（依頼本文）。
    pub task: Option<String>,
    /// progress だけ。
    pub detail: Option<String>,
    /// 終端だけ。
    pub duration_ms: Option<u64>,
    /// 終端だけ（結果本文）。
    pub result_text: Option<String>,
}

/// 省略可能な string field。欠落と null は None、string 以外は BadRequest。
fn opt_str(obj: &Value, key: &str) -> Result<Option<String>, FrameError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(FrameError::BadRequest),
    }
}

fn opt_u64(obj: &Value, key: &str) -> Result<Option<u64>, FrameError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(Some).ok_or(FrameError::BadRequest),
        Some(_) => Err(FrameError::BadRequest),
    }
}

pub(super) fn parse_tool_activity(obj: &Value) -> Result<ToolActivity, FrameError> {
    let depth = match obj.get("depth") {
        Some(Value::Number(n)) => n
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(FrameError::BadRequest)?,
        Some(_) => return Err(FrameError::BadRequest),
        None => return Err(FrameError::BadRequest),
    };
    let args = match obj.get("args") {
        None => Value::Null,
        Some(value) => value.clone(),
    };
    let result = match obj.get("result") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.clone()),
    };
    Ok(ToolActivity {
        session_key: super::nonempty_str(obj, "session")?,
        activity_id: nonempty_str(obj, "activity_id")?,
        call_id: nonempty_str(obj, "call_id")?,
        sub_session_id: opt_str(obj, "sub_session_id")?,
        tool: nonempty_str(obj, "tool")?,
        status: nonempty_str(obj, "status")?,
        depth,
        started_at: nonempty_str(obj, "started_at")?,
        duration_ms: opt_u64(obj, "duration_ms")?,
        args,
        result,
        error: opt_str(obj, "error")?,
    })
}

pub(super) fn parse_subtask_lifecycle(obj: &Value) -> Result<SubtaskLifecycle, FrameError> {
    Ok(SubtaskLifecycle {
        session_key: super::nonempty_str(obj, "session")?,
        run_id: nonempty_str(obj, "run_id")?,
        sub_session_id: opt_str(obj, "sub_session_id")?,
        status: nonempty_str(obj, "status")?,
        label: opt_str(obj, "label")?,
        task: opt_str(obj, "task")?,
        detail: opt_str(obj, "detail")?,
        duration_ms: opt_u64(obj, "duration_ms")?,
        result_text: opt_str(obj, "result_text")?,
    })
}
