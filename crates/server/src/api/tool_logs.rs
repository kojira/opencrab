//! `GET /api/agents/{id}/tool-logs`（載せ替え工程 5-b / RULINGS Q4）。
//!
//! `llm_logs` と同型: `?limit=`、配列 JSON、stats は作らない。

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ToolLogsQuery {
    pub limit: Option<i64>,
}

pub async fn list_tool_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ToolLogsQuery>,
) -> Json<serde_json::Value> {
    let limit = query.limit.unwrap_or(20);
    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::list_tool_logs(&conn, &id, limit) {
        Ok(logs) => {
            let data: Vec<serde_json::Value> = logs
                .into_iter()
                .map(|log| {
                    serde_json::json!({
                        "id": log.id,
                        "agent_id": log.agent_id,
                        "session_id": log.session_id,
                        "tool_name": log.tool_name,
                        "args_json": log.args_json,
                        "outcome": log.outcome,
                        "result_text": log.result_text,
                        "started_at": log.started_at,
                        "created_at": log.created_at,
                        "latency_ms": log.latency_ms,
                        "iteration": log.iteration,
                    })
                })
                .collect();
            Json(serde_json::json!(data))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_app_state;
    use axum::extract::{Path, Query, State};

    #[tokio::test]
    async fn list_returns_empty_array_for_unknown_agent() {
        let state = test_app_state();
        let Json(body) = list_tool_logs(
            State(state),
            Path("missing".into()),
            Query(ToolLogsQuery { limit: Some(10) }),
        )
        .await;
        assert_eq!(body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn list_returns_rows_newest_first() {
        let state = test_app_state();
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::insert_tool_log(
                &conn,
                &opencrab_db::queries::ToolLogWrite {
                    agent_id: "agent-t".into(),
                    session_id: Some("session-1".into()),
                    tool_name: "search_my_history".into(),
                    args_json: r#"{"query":"x"}"#.into(),
                    outcome: "done".into(),
                    result_text: r#"{"hits":1}"#.into(),
                    started_at: Some("2026-08-25T00:00:00Z".into()),
                    latency_ms: Some(11),
                    iteration: None,
                },
            )
            .unwrap();
            opencrab_db::queries::insert_tool_log(
                &conn,
                &opencrab_db::queries::ToolLogWrite {
                    agent_id: "agent-t".into(),
                    session_id: Some("session-1".into()),
                    tool_name: "execute_shell".into(),
                    args_json: "{}".into(),
                    outcome: "refused".into(),
                    result_text: "rejected: owner".into(),
                    started_at: Some("2026-08-25T00:00:01Z".into()),
                    latency_ms: Some(2),
                    iteration: None,
                },
            )
            .unwrap();
            opencrab_db::queries::insert_tool_log(
                &conn,
                &opencrab_db::queries::ToolLogWrite {
                    agent_id: "other".into(),
                    session_id: None,
                    tool_name: "other".into(),
                    args_json: "{}".into(),
                    outcome: "failed".into(),
                    result_text: "nope".into(),
                    started_at: None,
                    latency_ms: None,
                    iteration: None,
                },
            )
            .unwrap();
        }
        let Json(body) = list_tool_logs(
            State(state),
            Path("agent-t".into()),
            Query(ToolLogsQuery { limit: None }),
        )
        .await;
        let rows = body.as_array().expect("array");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["tool_name"], "execute_shell");
        assert_eq!(rows[0]["outcome"], "refused");
        assert_eq!(rows[1]["tool_name"], "search_my_history");
        assert_eq!(rows[1]["outcome"], "done");
        assert_eq!(rows[1]["session_id"], "session-1");
        assert_eq!(rows[1]["latency_ms"], 11);
        assert!(body.get("error").is_none());
    }
}
