use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult, SideEffect};

/// 発言アクション
pub struct SendSpeechAction;

#[async_trait]
impl Action for SendSpeechAction {
    fn name(&self) -> &str {
        "send_speech"
    }

    fn description(&self) -> &str {
        "メッセージを送信する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["content"],
            "properties": {
                "content": {
                    "type": "string",
                    "description": "送信するメッセージの内容"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ActionContext) -> ActionResult {
        let content = match args["content"].as_str() {
            Some(c) => c.to_string(),
            None => return ActionResult::error("content is required"),
        };

        ActionResult::success(json!({
            "sent": true,
            "content": content,
        }))
        .with_side_effect(SideEffect::MessageSent {
            channel: "default".to_string(),
            content,
        })
    }
}

/// 無反応アクション
pub struct SendNoreactAction;

#[async_trait]
impl Action for SendNoreactAction {
    fn name(&self) -> &str {
        "send_noreact"
    }

    fn description(&self) -> &str {
        "発言しない（何も反応しない）"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "発言しない理由"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ActionContext) -> ActionResult {
        let reason = args["reason"].as_str().unwrap_or("特になし");
        ActionResult::success(json!({
            "action": "noreact",
            "reason": reason,
        }))
    }
}

/// 心の声アクション
pub struct GenerateInnerVoiceAction;

#[async_trait]
impl Action for GenerateInnerVoiceAction {
    fn name(&self) -> &str {
        "generate_inner_voice"
    }

    fn description(&self) -> &str {
        "心の声を記録する（他の参加者には見えない内省）"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["thought"],
            "properties": {
                "thought": {
                    "type": "string",
                    "description": "心の声の内容"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let thought = match args["thought"].as_str() {
            Some(t) => t,
            None => return ActionResult::error("thought is required"),
        };

        // セッションログに記録
        if let Some(session_id) = &ctx.session_id {
            let log = opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: ctx.agent_id.clone(),
                session_id: session_id.clone(),
                log_type: "inner_voice".to_string(),
                content: thought.to_string(),
                speaker_id: Some(ctx.agent_id.clone()),
                turn_number: None,
                metadata_json: None,
            };
            if let Ok(conn) = ctx.db.lock() {
                let _ = opencrab_db::queries::insert_session_log(&conn, &log);
            }
        }

        ActionResult::success(json!({
            "recorded": true,
            "thought": thought,
        }))
    }
}

/// 心象更新アクション
pub struct UpdateImpressionAction;

#[async_trait]
impl Action for UpdateImpressionAction {
    fn name(&self) -> &str {
        "update_impression"
    }

    fn description(&self) -> &str {
        "他の参加者への印象を更新する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["target_id", "target_name"],
            "properties": {
                "target_id": {
                    "type": "string",
                    "description": "対象者のID"
                },
                "target_name": {
                    "type": "string",
                    "description": "対象者の名前"
                },
                "personality": {
                    "type": "string",
                    "description": "性格の印象"
                },
                "communication_style": {
                    "type": "string",
                    "description": "コミュニケーションスタイルの印象"
                },
                "recent_behavior": {
                    "type": "string",
                    "description": "最近の行動の印象"
                },
                "agreement": {
                    "type": "string",
                    "description": "意見の一致度（同意/中立/反対）"
                },
                "notes": {
                    "type": "string",
                    "description": "その他のメモ"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let target_id = match args["target_id"].as_str() {
            Some(t) => t,
            None => return ActionResult::error("target_id is required"),
        };
        let target_name = match args["target_name"].as_str() {
            Some(t) => t,
            None => return ActionResult::error("target_name is required"),
        };

        let session_id = ctx.session_id.clone().unwrap_or_default();

        let impression = opencrab_db::queries::ImpressionRow {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: ctx.agent_id.clone(),
            session_id,
            target_id: target_id.to_string(),
            target_name: target_name.to_string(),
            personality: args["personality"].as_str().unwrap_or("").to_string(),
            communication_style: args["communication_style"].as_str().unwrap_or("").to_string(),
            recent_behavior: args["recent_behavior"].as_str().unwrap_or("").to_string(),
            agreement: args["agreement"].as_str().unwrap_or("中立").to_string(),
            notes: args["notes"].as_str().unwrap_or("").to_string(),
            last_updated_turn: 0,
        };

        if let Ok(conn) = ctx.db.lock() {
            if let Err(e) = opencrab_db::queries::upsert_impression(&conn, &impression) {
                return ActionResult::error(&format!("Failed to update impression: {e}"));
            }
        }

        ActionResult::success(json!({
            "updated": true,
            "target": target_name,
        }))
    }
}

/// システム情報取得アクション
pub struct GetSystemInfoAction;

#[async_trait]
impl Action for GetSystemInfoAction {
    fn name(&self) -> &str {
        "get_system_info"
    }

    fn description(&self) -> &str {
        "自分のシステム情報（使用中のLLMモデル、プロバイダー、ゲートウェイなど）を確認する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let info = ctx.runtime_info.lock().unwrap().clone();
        let active = info.active_model.unwrap_or_else(|| info.default_model.clone());

        ActionResult::success(json!({
            "agent_id": ctx.agent_id,
            "agent_name": ctx.agent_name,
            "default_model": info.default_model,
            "active_model": active,
            "available_providers": info.available_providers,
            "gateway": info.gateway,
        }))
    }
}

/// 議論終了宣言
pub struct DeclareDoneAction;

#[async_trait]
impl Action for DeclareDoneAction {
    fn name(&self) -> &str {
        "declare_done"
    }

    fn description(&self) -> &str {
        "議論の終了を宣言する（これ以上意見がないことを示す）"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "終了する理由"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ActionContext) -> ActionResult {
        let reason = args["reason"].as_str().unwrap_or("議論が十分に行われた");
        ActionResult::success(json!({
            "done": true,
            "reason": reason,
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
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn test_send_speech_success() {
        let (_dir, ctx) = test_context();
        let result = SendSpeechAction.execute(&json!({"content": "hello"}), &ctx).await;
        assert!(result.success);
        assert!(
            result.side_effects.iter().any(|e| matches!(e, SideEffect::MessageSent { .. })),
            "Expected MessageSent side effect"
        );
    }

    #[tokio::test]
    async fn test_send_speech_missing_content() {
        let (_dir, ctx) = test_context();
        let result = SendSpeechAction.execute(&json!({}), &ctx).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn test_send_noreact() {
        let (_dir, ctx) = test_context();
        let result = SendNoreactAction.execute(&json!({"reason": "thinking"}), &ctx).await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_declare_done() {
        let (_dir, ctx) = test_context();
        let result = DeclareDoneAction.execute(&json!({"reason": "done"}), &ctx).await;
        assert!(result.success);
    }
}
