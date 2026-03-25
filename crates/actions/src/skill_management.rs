use async_trait::async_trait;
use serde_json::json;
use uuid;

use crate::traits::{Action, ActionContext, ActionResult, SideEffect};

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
            opencrab_db::queries::find_skill_by_name_any(&conn, &ctx.agent_id, name)
                .ok()
                .flatten()
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
        assert!(result
            .side_effects
            .iter()
            .any(|e| matches!(e, SideEffect::SkillAcquired { .. })));
        assert!(result
            .side_effects
            .iter()
            .any(|e| matches!(e, SideEffect::FileWritten { .. })));

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
        let content = ctx
            .workspace
            .read("skills/file-check.skill.md")
            .await
            .unwrap();
        assert!(content.contains("File Check"));
        assert!(content.contains("guide"));
        assert!(content.contains("pattern"));
    }
}
