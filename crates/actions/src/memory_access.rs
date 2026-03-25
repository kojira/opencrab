use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

/// 記憶インデックスのツリー構造を閲覧するアクション
pub struct BrowseMemoryIndexAction;

#[async_trait]
impl Action for BrowseMemoryIndexAction {
    fn name(&self) -> &str {
        "browse_memory_index"
    }

    fn description(&self) -> &str {
        "記憶インデックスのツリー構造を閲覧する。タイトルと要約のみのコンパクト表示。関連しそうなノードを見つけたら retrieve_memory_nodes で全文を取得できる。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "max_depth": {
                    "type": "integer",
                    "description": "表示する最大深さ（デフォルト: 3）",
                    "default": 3
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let max_depth = args["max_depth"].as_i64().unwrap_or(3) as i32;

        let tree = if let Ok(conn) = ctx.db.lock() {
            match opencrab_db::queries::get_index_tree(&conn, &ctx.agent_id) {
                Ok(nodes) => nodes,
                Err(e) => return ActionResult::error(&format!("Failed to get index tree: {e}")),
            }
        } else {
            return ActionResult::error("Failed to acquire DB lock");
        };

        let filtered: Vec<serde_json::Value> = tree
            .iter()
            .filter(|n| n.depth <= max_depth)
            .map(|n| {
                json!({
                    "node_id": n.id,
                    "parent_id": n.parent_id,
                    "node_type": n.node_type,
                    "title": n.title,
                    "summary": n.summary,
                    "depth": n.depth,
                    "child_count": n.child_count,
                    "start_log_id": n.start_log_id,
                    "end_log_id": n.end_log_id,
                })
            })
            .collect();

        ActionResult::success(json!({
            "node_count": filtered.len(),
            "tree": filtered,
        }))
    }
}

/// 記憶インデックスノードの全文テキストを取得するアクション
pub struct RetrieveMemoryNodesAction;

#[async_trait]
impl Action for RetrieveMemoryNodesAction {
    fn name(&self) -> &str {
        "retrieve_memory_nodes"
    }

    fn description(&self) -> &str {
        "browse_memory_indexで見つけたノードの全文テキストを取得する。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["node_ids"],
            "properties": {
                "node_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "取得するノードIDのリスト（1-5個）",
                    "minItems": 1,
                    "maxItems": 5
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let node_ids: Vec<String> = match args["node_ids"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => return ActionResult::error("node_ids is required (array of strings)"),
        };

        if node_ids.is_empty() {
            return ActionResult::error("node_ids must not be empty");
        }
        if node_ids.len() > 5 {
            return ActionResult::error("Maximum 5 node_ids allowed");
        }

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };

        let mut results: Vec<serde_json::Value> = Vec::new();

        for node_id in &node_ids {
            let node = match opencrab_db::queries::get_index_node(&conn, node_id) {
                Ok(Some(n)) => n,
                Ok(None) => {
                    results.push(json!({
                        "node_id": node_id,
                        "error": "Node not found",
                    }));
                    continue;
                }
                Err(e) => {
                    results.push(json!({
                        "node_id": node_id,
                        "error": format!("Query error: {e}"),
                    }));
                    continue;
                }
            };

            // ログIDレンジがある場合は全文取得
            let messages = if let (Some(start), Some(end)) = (node.start_log_id, node.end_log_id) {
                match opencrab_db::queries::get_session_logs_by_id_range(
                    &conn,
                    &ctx.agent_id,
                    start,
                    end,
                ) {
                    Ok(logs) => logs
                        .iter()
                        .map(|l| {
                            json!({
                                "speaker": l.speaker_id.as_deref().unwrap_or("unknown"),
                                "content": l.content,
                                "log_type": l.log_type,
                            })
                        })
                        .collect::<Vec<_>>(),
                    Err(_) => vec![],
                }
            } else {
                vec![]
            };

            results.push(json!({
                "node_id": node.id,
                "title": node.title,
                "summary": node.summary,
                "node_type": node.node_type,
                "messages": messages,
            }));
        }

        ActionResult::success(json!({
            "nodes": results,
        }))
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
            db: std::sync::Arc::new(std::sync::Mutex::new(conn)),
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

    // ---- BrowseMemoryIndexAction ----

    #[tokio::test]
    async fn test_browse_memory_index_empty() {
        let (_dir, ctx) = test_context();
        let result = BrowseMemoryIndexAction.execute(&json!({}), &ctx).await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert_eq!(data["node_count"], 0);
    }

    #[tokio::test]
    async fn test_browse_memory_index_with_nodes() {
        let (_dir, ctx) = test_context();
        {
            let conn = ctx.db.lock().unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            let node = opencrab_db::queries::IndexNodeRow {
                id: "root-agent-1".to_string(),
                agent_id: "agent-1".to_string(),
                parent_id: None,
                node_type: "root".to_string(),
                source_type: "session_log".to_string(),
                title: "Memory Root".to_string(),
                summary: "Root node".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 1,
                token_count: 0,
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            opencrab_db::queries::insert_index_node(&conn, &node).unwrap();
            let topic = opencrab_db::queries::IndexNodeRow {
                id: "topic-1".to_string(),
                agent_id: "agent-1".to_string(),
                parent_id: Some("root-agent-1".to_string()),
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: "Rust Discussion".to_string(),
                summary: "A discussion about Rust.".to_string(),
                start_log_id: Some(1),
                end_log_id: Some(5),
                source_session_id: Some("session-1".to_string()),
                date_from: None,
                date_to: None,
                depth: 1,
                child_count: 0,
                token_count: 100,
                created_at: now.clone(),
                updated_at: now,
            };
            opencrab_db::queries::insert_index_node(&conn, &topic).unwrap();
        }
        let result = BrowseMemoryIndexAction.execute(&json!({}), &ctx).await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert_eq!(data["node_count"], 2);
        let tree = data["tree"].as_array().unwrap();
        assert!(tree.iter().any(|n| n["title"] == "Rust Discussion"));
    }

    // ---- RetrieveMemoryNodesAction ----

    #[tokio::test]
    async fn test_retrieve_memory_nodes_missing_ids() {
        let (_dir, ctx) = test_context();
        let result = RetrieveMemoryNodesAction.execute(&json!({}), &ctx).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_retrieve_memory_nodes_not_found() {
        let (_dir, ctx) = test_context();
        let result = RetrieveMemoryNodesAction
            .execute(&json!({"node_ids": ["nonexistent"]}), &ctx)
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        let nodes = data["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0]["error"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn test_retrieve_memory_nodes_with_logs() {
        let (_dir, ctx) = test_context();
        {
            let conn = ctx.db.lock().unwrap();
            // Insert log
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: "agent-1".to_string(),
                session_id: "session-1".to_string(),
                log_type: "message".to_string(),
                content: "Hello from test".to_string(),
                speaker_id: Some("user-1".to_string()),
                turn_number: Some(1),
                metadata_json: None,
                created_at: None,
            };
            opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
            // Insert index node
            let now = chrono::Utc::now().to_rfc3339();
            let node = opencrab_db::queries::IndexNodeRow {
                id: "topic-test".to_string(),
                agent_id: "agent-1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: "Test Topic".to_string(),
                summary: "Test summary".to_string(),
                start_log_id: Some(1),
                end_log_id: Some(1),
                source_session_id: Some("session-1".to_string()),
                date_from: None,
                date_to: None,
                depth: 3,
                child_count: 0,
                token_count: 10,
                created_at: now.clone(),
                updated_at: now,
            };
            opencrab_db::queries::insert_index_node(&conn, &node).unwrap();
        }
        let result = RetrieveMemoryNodesAction
            .execute(&json!({"node_ids": ["topic-test"]}), &ctx)
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        let nodes = data["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["title"], "Test Topic");
        let messages = nodes[0]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], "Hello from test");
    }
}
