use async_trait::async_trait;
use serde_json::json;

// `CallerIdentity` は本体では未使用だが、`mod tests` が `use super::*` 経由で使う。
#[allow(unused_imports)]
use crate::traits::{Action, ActionContext, ActionResult, CallerIdentity};

/// instructionsを更新するアクション（Ownerのみ実行可能）
pub struct UpdateInstructionsAction;

#[async_trait]
impl Action for UpdateInstructionsAction {
    fn name(&self) -> &str {
        "update_instructions"
    }

    fn description(&self) -> &str {
        "自分のinstructionsを更新する（ownerへの返信時のみ使用可能）。instructionsはシステムプロンプトに毎回展開される操作ルール・行動指針。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["instructions"],
            "properties": {
                "instructions": {
                    "type": "string",
                    "description": "新しいinstructionsの内容（AGENTS.md相当）"
                },
                "reason": {
                    "type": "string",
                    "description": "更新する理由"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        // owner 等価判定（#485: co_agent も owner 等価。唯一の源は is_owner_equivalent）。
        if !ctx.caller.is_owner_equivalent() {
            return ActionResult::error(
                "update_instructions はowner（または owner 等価の co_agent）からのメッセージへの返信時のみ実行可能です",
            );
        }

        let instructions = match args["instructions"].as_str() {
            Some(s) => s.to_string(),
            None => return ActionResult::error("instructions is required"),
        };
        let reason = args["reason"].as_str().unwrap_or("（理由なし）");

        // DBに保存
        let conn = ctx.db.lock().unwrap();

        let agent = match opencrab_db::queries::get_agent(&conn, &ctx.agent_id) {
            Ok(Some(a)) => a,
            Ok(None) => return ActionResult::error("agent not found"),
            Err(e) => return ActionResult::error(&format!("DB error: {e}")),
        };

        let updated = opencrab_db::queries::AgentRow {
            instructions: instructions.clone(),
            ..agent
        };

        if let Err(e) = opencrab_db::queries::upsert_agent(&conn, &updated) {
            return ActionResult::error(&format!("Failed to save instructions: {e}"));
        }

        ActionResult::success(json!({
            "updated": true,
            "reason": reason,
            "instructions_length": instructions.len(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::{upsert_agent, AgentRow};
    use serde_json::json;

    fn make_context(caller: CallerIdentity) -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        // soulレコードを作成
        let agent = AgentRow {
            agent_id: "agent-1".to_string(),
            name: "Test Agent".to_string(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "テスト".to_string(),
            personality: None,
            instructions: "".to_string(),
            heartbeat_instructions: "".to_string(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        };
        upsert_agent(&conn, &agent).unwrap();

        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            caller,
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
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn test_owner_can_update_instructions() {
        let (_dir, ctx) = make_context(CallerIdentity::Owner);
        let action = UpdateInstructionsAction;
        let result = action
            .execute(
                &json!({"instructions": "新しいルール", "reason": "テスト"}),
                &ctx,
            )
            .await;
        assert!(
            result.success,
            "Owner should be able to update instructions"
        );
        assert_eq!(result.data.unwrap()["updated"], true);

        // DBに保存されたか確認
        let conn = ctx.db.lock().unwrap();
        let agent = opencrab_db::queries::get_agent(&conn, "agent-1")
            .unwrap()
            .unwrap();
        assert_eq!(agent.instructions, "新しいルール");
    }

    #[tokio::test]
    async fn test_agent_cannot_update_instructions() {
        let (_dir, ctx) = make_context(CallerIdentity::Agent);
        let action = UpdateInstructionsAction;
        let result = action
            .execute(&json!({"instructions": "乗っ取り", "reason": "悪意"}), &ctx)
            .await;
        assert!(
            !result.success,
            "Agent should NOT be able to update instructions"
        );
        assert!(result.error.unwrap().contains("owner"));
    }

    #[tokio::test]
    async fn test_trusted_user_cannot_update_instructions() {
        let (_dir, ctx) = make_context(CallerIdentity::TrustedUser);
        let action = UpdateInstructionsAction;
        let result = action
            .execute(&json!({"instructions": "不正変更", "reason": "悪意"}), &ctx)
            .await;
        assert!(
            !result.success,
            "TrustedUser should NOT be able to update instructions"
        );
    }

    #[tokio::test]
    async fn test_missing_instructions_param() {
        let (_dir, ctx) = make_context(CallerIdentity::Owner);
        let action = UpdateInstructionsAction;
        let result = action.execute(&json!({}), &ctx).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("required"));
    }
}
