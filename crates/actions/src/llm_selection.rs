use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult, SideEffect};

/// LLM選択アクション — エージェントが自ら使用モデルを切り替える
pub struct SelectLlmAction;

#[async_trait]
impl Action for SelectLlmAction {
    fn name(&self) -> &str {
        "select_llm"
    }

    fn description(&self) -> &str {
        "タスクに応じて使用するLLMモデルを切り替える。provider:model形式（例: openai:gpt-4o-mini）またはエイリアス（fast, smart等）で指定。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["model_alias", "reason"],
            "properties": {
                "purpose": {
                    "type": "string",
                    "description": "LLMの用途（自由記述。例: 複雑な推論, 簡単な質問応答, 創作, コード生成, 要約）"
                },
                "model_alias": {
                    "type": "string",
                    "description": "使用するモデル（provider:model形式またはエイリアス。例: openai:gpt-4o-mini, openrouter:openai/gpt-4o-mini, fast, smart）"
                },
                "reason": {
                    "type": "string",
                    "description": "このモデルを選んだ理由（自由記述。過去の経験やメトリクスに基づく判断を書く）"
                },
                "duration": {
                    "type": "string",
                    "description": "この設定の有効期間（自由記述。例: this_turn, this_session, until_task_complete）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let purpose = args["purpose"].as_str().unwrap_or("conversation");
        let model_alias = args["model_alias"].as_str().unwrap_or("smart");
        let reason = args["reason"].as_str().unwrap_or("");
        let duration = args["duration"].as_str().unwrap_or("this_turn");

        // Update the shared model_override so SkillEngine uses this model.
        if let Ok(mut current) = ctx.model_override.lock() {
            *current = Some(model_alias.to_string());
        }

        // Update the shared current_purpose so metrics are tagged correctly.
        if let Ok(mut current) = ctx.current_purpose.lock() {
            *current = purpose.to_string();
        }

        ActionResult::success(json!({
            "switched": true,
            "selected": model_alias,
            "purpose": purpose,
            "reason": reason,
            "duration": duration,
        }))
        .with_side_effect(SideEffect::LlmSwitched {
            purpose: purpose.to_string(),
            model: model_alias.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: Arc::new(std::sync::Mutex::new(conn)),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
            model_override: Arc::new(std::sync::Mutex::new(None)),
            current_purpose: Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn test_select_llm_updates_model_override() {
        let (_dir, ctx) = test_context();
        let action = SelectLlmAction;

        let result = action
            .execute(
                &json!({
                    "model_alias": "openai:gpt-4o-mini",
                    "reason": "Cheaper for simple tasks",
                    "purpose": "conversation",
                }),
                &ctx,
            )
            .await;

        assert!(result.success);
        assert_eq!(result.data.as_ref().unwrap()["switched"], true);
        assert_eq!(result.data.as_ref().unwrap()["selected"], "openai:gpt-4o-mini");

        // Verify model_override was updated.
        let override_val = ctx.model_override.lock().unwrap();
        assert_eq!(override_val.as_deref(), Some("openai:gpt-4o-mini"));
    }

    #[tokio::test]
    async fn test_select_llm_emits_side_effect() {
        let (_dir, ctx) = test_context();
        let action = SelectLlmAction;

        let result = action
            .execute(
                &json!({
                    "model_alias": "fast",
                    "reason": "Speed needed",
                }),
                &ctx,
            )
            .await;

        assert_eq!(result.side_effects.len(), 1);
        match &result.side_effects[0] {
            SideEffect::LlmSwitched { purpose, model } => {
                assert_eq!(model, "fast");
                assert_eq!(purpose, "conversation");
            }
            _ => panic!("Expected LlmSwitched side effect"),
        }
    }
}
