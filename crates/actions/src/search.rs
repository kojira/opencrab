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
        match ctx.workspace.write(&file_path, &skill_content).await {
            Ok(_) => {
                // DBにも登録
                let skill_id = uuid::Uuid::new_v4().to_string();
                let skill = opencrab_db::queries::SkillRow {
                    id: skill_id.clone(),
                    agent_id: ctx.agent_id.clone(),
                    name: name.to_string(),
                    description: args["description"].as_str().unwrap_or("").to_string(),
                    situation_pattern: args["situation_pattern"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                    guidance: args["guidance"].as_str().unwrap_or("").to_string(),
                    source_type: "self_created".to_string(),
                    source_context: None,
                    file_path: Some(file_path.clone()),
                    effectiveness: None,
                    usage_count: 0,
                    is_active: true,
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
}
