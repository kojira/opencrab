use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult, SideEffect};

/// 自分の履歴を検索するアクション
pub struct SearchMyHistoryAction;

#[async_trait]
impl Action for SearchMyHistoryAction {
    fn name(&self) -> &str {
        "search_my_history"
    }

    fn description(&self) -> &str {
        "自分の過去のやりとりを検索する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "検索クエリ"
                },
                "limit": {
                    "type": "integer",
                    "description": "取得件数（デフォルト: 10）",
                    "default": 10
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let query = match args["query"].as_str() {
            Some(q) => q,
            None => return ActionResult::error("query is required"),
        };
        let limit = args["limit"].as_u64().unwrap_or(10) as usize;

        let results = if let Ok(conn) = ctx.db.lock() {
            match opencrab_db::queries::search_session_logs(&conn, &ctx.agent_id, query, limit) {
                Ok(r) => r,
                Err(e) => return ActionResult::error(&format!("Search failed: {e}")),
            }
        } else {
            return ActionResult::error("Failed to acquire DB lock");
        };

        ActionResult::success(json!({
            "query": query,
            "count": results.len(),
            "results": results,
        }))
    }
}

/// 要約して保存するアクション
pub struct SummarizeAndSaveAction;

#[async_trait]
impl Action for SummarizeAndSaveAction {
    fn name(&self) -> &str {
        "summarize_and_save"
    }

    fn description(&self) -> &str {
        "内容を要約してワークスペースに保存する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["content", "filename"],
            "properties": {
                "content": {
                    "type": "string",
                    "description": "保存する要約内容"
                },
                "filename": {
                    "type": "string",
                    "description": "保存先ファイル名（相対パス）"
                },
                "summary_type": {
                    "type": "string",
                    "enum": ["session", "topic", "research", "note"],
                    "description": "要約の種類"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let content = match args["content"].as_str() {
            Some(c) => c,
            None => return ActionResult::error("content is required"),
        };
        let filename = match args["filename"].as_str() {
            Some(f) => f,
            None => return ActionResult::error("filename is required"),
        };

        match ctx.workspace.write(filename, content).await {
            Ok(_) => ActionResult::success(json!({
                "saved": true,
                "filename": filename,
            }))
            .with_side_effect(SideEffect::FileWritten {
                path: filename.to_string(),
            }),
            Err(e) => ActionResult::error(&e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::*;
    use serde_json::json;

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: opencrab_db::Db::from_connection(conn),
            workspace: std::sync::Arc::new(ws),
            last_metrics_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
            model_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
            current_purpose: std::sync::Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: std::sync::Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
            caller: CallerIdentity::Owner,
        };
        (dir, ctx)
    }

    // ---- SearchMyHistoryAction ----

    #[tokio::test]
    async fn test_search_my_history_missing_query() {
        let (_dir, ctx) = test_context();
        let result = SearchMyHistoryAction.execute(&json!({}), &ctx).await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("query is required"));
    }

    #[tokio::test]
    async fn test_search_my_history_empty_results() {
        let (_dir, ctx) = test_context();
        let result = SearchMyHistoryAction
            .execute(&json!({"query": "nonexistent"}), &ctx)
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert_eq!(data["count"], 0);
    }

    #[tokio::test]
    async fn test_search_my_history_with_data() {
        let (_dir, ctx) = test_context();
        {
            let conn = ctx.db.lock().unwrap();
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "agent-1".to_string(),
                session_id: "session-1".to_string(),
                log_type: "message".to_string(),
                content: "Rust programming is wonderful".to_string(),
                speaker_id: Some("agent-1".to_string()),
                turn_number: Some(1),
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
        }
        let result = SearchMyHistoryAction
            .execute(&json!({"query": "Rust", "limit": 5}), &ctx)
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert!(data["count"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn test_search_my_history_custom_limit() {
        let (_dir, ctx) = test_context();
        let result = SearchMyHistoryAction
            .execute(&json!({"query": "anything", "limit": 3}), &ctx)
            .await;
        assert!(result.success);
        assert_eq!(result.data.unwrap()["count"], 0);
    }
}
