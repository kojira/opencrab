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

/// 自作スキル作成アクション
pub struct CreateMySkillAction;

#[async_trait]
impl Action for CreateMySkillAction {
    fn name(&self) -> &str {
        "create_my_skill"
    }

    fn description(&self) -> &str {
        "学んだことを正式なスキルファイルとして保存する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["name", "description", "situation_pattern", "guidance"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "スキル名"
                },
                "description": {
                    "type": "string",
                    "description": "スキルの説明"
                },
                "situation_pattern": {
                    "type": "string",
                    "description": "スキルが適用できる状況パターン"
                },
                "guidance": {
                    "type": "string",
                    "description": "具体的な行動指針"
                },
                "actions": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "関連するアクション名のリスト"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let name = match args["name"].as_str() {
            Some(n) => n,
            None => return ActionResult::error("name is required"),
        };

        let actions: Vec<String> = args["actions"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let skill_content = format!(
            "---\nname: {name}\ndescription: \"{desc}\"\nversion: 1\nactions:\n{actions_yaml}\n---\n\n# {name}\n\n## 状況パターン\n{pattern}\n\n## 行動指針\n{guidance}\n",
            name = name,
            desc = args["description"].as_str().unwrap_or(""),
            actions_yaml = actions
                .iter()
                .map(|a| format!("  - {a}"))
                .collect::<Vec<_>>()
                .join("\n"),
            pattern = args["situation_pattern"].as_str().unwrap_or(""),
            guidance = args["guidance"].as_str().unwrap_or(""),
        );

        let file_path = format!("skills/{}.skill.md", name.replace(' ', "-").to_lowercase());
        let description = args["description"].as_str().unwrap_or("").to_string();
        let situation_pattern = args["situation_pattern"].as_str().unwrap_or("").to_string();
        let guidance = args["guidance"].as_str().unwrap_or("").to_string();

        // Check if skill with same name already exists (including archived)
        let existing = ctx.db.lock().ok().and_then(|conn| {
            opencrab_db::queries::find_skill_by_name_any(&conn, &ctx.agent_id, name).ok().flatten()
        });

        if let Some(existing) = existing {
            let was_archived = existing.archived;
            let skill_id = existing.id.clone();

            let mut updated = existing;
            updated.description = description;
            updated.situation_pattern = situation_pattern;
            updated.guidance = guidance;
            updated.file_path = Some(file_path.clone());
            updated.is_active = true;
            updated.archived = false;

            if let Ok(conn) = ctx.db.lock() {
                let _ = opencrab_db::queries::update_skill(&conn, &updated);
            }

            // Overwrite the skill file
            match ctx.workspace.write(&file_path, &skill_content).await {
                Ok(_) => {
                    let result_key = if was_archived { "restored" } else { "updated" };
                    ActionResult::success(json!({
                        result_key: true,
                        "skill_id": skill_id,
                        "file_path": file_path,
                    }))
                    .with_side_effect(SideEffect::FileWritten { path: file_path })
                }
                Err(e) => ActionResult::error(&e.to_string()),
            }
        } else {
            match ctx.workspace.write(&file_path, &skill_content).await {
                Ok(_) => {
                    // DBにも登録
                    let skill_id = uuid::Uuid::new_v4().to_string();
                    let skill = opencrab_db::queries::SkillRow {
                        id: skill_id.clone(),
                        agent_id: ctx.agent_id.clone(),
                        name: name.to_string(),
                        description,
                        situation_pattern,
                        guidance,
                        source_type: "self_created".to_string(),
                        source_context: None,
                        file_path: Some(file_path.clone()),
                        effectiveness: None,
                        usage_count: 0,
                        is_active: true,
                        permission: "\"agent\"".to_string(),
                        archived: false,
                    };

                    if let Ok(conn) = ctx.db.lock() {
                        let _ = opencrab_db::queries::insert_skill(&conn, &skill);
                    }

                    ActionResult::success(json!({
                        "created": true,
                        "skill_id": skill_id,
                        "file_path": file_path,
                    }))
                    .with_side_effect(SideEffect::SkillAcquired { skill_id })
                    .with_side_effect(SideEffect::FileWritten { path: file_path })
                }
                Err(e) => ActionResult::error(&e.to_string()),
            }
        }
    }
}

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
            Some(arr) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
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

    // ---- CreateMySkillAction ----

    #[tokio::test]
    async fn test_create_my_skill_success() {
        let (_dir, ctx) = test_context();
        let result = CreateMySkillAction
            .execute(
                &json!({
                    "name": "Test Skill",
                    "description": "A test skill",
                    "situation_pattern": "when testing",
                    "guidance": "Be thorough",
                    "actions": ["ws_read", "ws_write"]
                }),
                &ctx,
            )
            .await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert!(data["created"].as_bool().unwrap());
        assert!(data["skill_id"].as_str().is_some());
        assert!(data["file_path"].as_str().unwrap().contains("skills/"));

        // Verify side effects
        assert!(result.side_effects.iter().any(|e| matches!(e, SideEffect::SkillAcquired { .. })));
        assert!(result.side_effects.iter().any(|e| matches!(e, SideEffect::FileWritten { .. })));

        // Verify DB insertion
        let conn = ctx.db.lock().unwrap();
        let skills = opencrab_db::queries::list_skills(&conn, "agent-1", true).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "Test Skill");
        assert_eq!(skills[0].source_type, "self_created");
    }

    #[tokio::test]
    async fn test_create_my_skill_missing_name() {
        let (_dir, ctx) = test_context();
        let result = CreateMySkillAction
            .execute(&json!({"description": "no name"}), &ctx)
            .await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("name is required"));
    }

    #[tokio::test]
    async fn test_create_my_skill_file_content() {
        let (_dir, ctx) = test_context();
        CreateMySkillAction
            .execute(
                &json!({
                    "name": "File Check",
                    "description": "desc",
                    "situation_pattern": "pattern",
                    "guidance": "guide"
                }),
                &ctx,
            )
            .await;
        let content = ctx.workspace.read("skills/file-check.skill.md").await.unwrap();
        assert!(content.contains("File Check"));
        assert!(content.contains("guide"));
        assert!(content.contains("pattern"));
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
                title: "Memory Root".to_string(),
                summary: "Root node".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
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
                title: "Rust Discussion".to_string(),
                summary: "A discussion about Rust.".to_string(),
                start_log_id: Some(1),
                end_log_id: Some(5),
                source_session_id: Some("session-1".to_string()),
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
            };
            opencrab_db::queries::insert_session_log(&conn, &log).unwrap();
            // Insert index node
            let now = chrono::Utc::now().to_rfc3339();
            let node = opencrab_db::queries::IndexNodeRow {
                id: "topic-test".to_string(),
                agent_id: "agent-1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                title: "Test Topic".to_string(),
                summary: "Test summary".to_string(),
                start_log_id: Some(1),
                end_log_id: Some(1),
                source_session_id: Some("session-1".to_string()),
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
