//! スリープ棚卸しの監査ログ参照 API（ダッシュボードのスリープ履歴ビュー用）。
//!
//! 層1（構造化監査）は `agent_logs`（context="sleep"）に JSON で保存されている。
//! ここではそれを取り出してパースし、フロントに構造化して返す。生プロンプト/生応答
//! （層2）は `llm_log_ids` 経由で既存の LLM ログ画面から辿れる。

use axum::extract::{Path, State};
use axum::Json;
use serde_json::json;

use crate::AppState;

/// GET /api/agents/{id}/sleep-logs — スリープ棚卸しの履歴（新しい順）。
pub async fn get_sleep_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(_) => return Json(json!({ "logs": [] })),
    };
    let rows =
        opencrab_db::queries::list_agent_logs(&conn, Some(&id), None, 100).unwrap_or_default();

    let logs: Vec<serde_json::Value> = rows
        .into_iter()
        .filter(|r| r.context == "sleep")
        .map(|r| {
            // message は層1の構造化 JSON。壊れていても created_at は返す。
            let parsed: serde_json::Value =
                serde_json::from_str(&r.message).unwrap_or(serde_json::Value::Null);
            json!({
                "id": r.id,
                "created_at": r.created_at,
                "audit": parsed,
            })
        })
        .collect();

    Json(json!({ "logs": logs }))
}
