use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

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
                created_at: None,
            };
            if let Ok(conn) = ctx.db.lock() {
                opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
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
            communication_style: args["communication_style"]
                .as_str()
                .unwrap_or("")
                .to_string(),
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
        let active = info
            .active_model
            .unwrap_or_else(|| info.default_model.clone());

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

/// 到達点チェックポイントの明示更新（#825 / #826-B）。
pub struct UpdateContextCheckpointAction;

#[async_trait]
impl Action for UpdateContextCheckpointAction {
    fn name(&self) -> &str {
        "update_context_checkpoint"
    }

    fn description(&self) -> &str {
        "到達点チェックポイントを更新する。schema は {confirmed, position, next}。1000 token を超える更新は失敗し、旧値を残す。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["confirmed", "position", "next"],
            "properties": {
                "confirmed": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "確認済みの到達点"
                },
                "position": {
                    "type": "string",
                    "description": "いまの位置"
                },
                "next": {
                    "type": "string",
                    "description": "次にすること"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let Some(session_id) = ctx.session_id.as_deref() else {
            return ActionResult::error("session_id is required");
        };
        let confirmed = match args["confirmed"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>(),
            None => return ActionResult::error("confirmed is required"),
        };
        let Some(position) = args["position"].as_str() else {
            return ActionResult::error("position is required");
        };
        let Some(next) = args["next"].as_str() else {
            return ActionResult::error("next is required");
        };
        let incoming = opencrab_core::context_budget::ContextCheckpoint {
            confirmed,
            position: position.to_string(),
            next: next.to_string(),
        };
        let Ok(conn) = ctx.db.lock() else {
            return ActionResult::error("db lock failed");
        };
        let logs = match opencrab_db::queries::list_session_logs_by_session(&conn, session_id) {
            Ok(logs) => logs,
            Err(e) => {
                return ActionResult::error(&format!("failed to read previous checkpoint: {e}"))
            }
        };
        let previous = logs.iter().rev().find_map(|log| {
            if log.log_type == "system" {
                opencrab_core::context_budget::parse_checkpoint_event(&log.content)
            } else {
                None
            }
        });
        match opencrab_core::context_budget::apply_explicit_checkpoint(previous.as_ref(), incoming)
        {
            Ok(cp) => {
                let log = opencrab_db::queries::SessionLogRow {
                    id: None,
                    agent_id: ctx.agent_id.clone(),
                    session_id: session_id.to_string(),
                    log_type: "system".to_string(),
                    content: opencrab_core::context_budget::checkpoint_event_body(&cp),
                    speaker_id: Some(ctx.agent_id.clone()),
                    turn_number: None,
                    metadata_json: None,
                    created_at: None,
                };
                if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
                    return ActionResult::error(&format!("failed to persist checkpoint: {e}"));
                }
                ActionResult::success(json!({
                    "updated": true,
                    "confirmed": cp.confirmed,
                    "position": cp.position,
                    "next": cp.next,
                }))
            }
            Err(reason) => ActionResult::error(reason.as_str()),
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
            caller: CallerIdentity::Owner,
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
    async fn test_declare_done() {
        let (_dir, ctx) = test_context();
        let result = DeclareDoneAction
            .execute(&json!({"reason": "done"}), &ctx)
            .await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn update_context_checkpoint_writes_typed_event() {
        let (_dir, ctx) = test_context();
        let result = UpdateContextCheckpointAction
            .execute(
                &json!({
                    "confirmed": ["step-1"],
                    "position": "waiting",
                    "next": "confirm"
                }),
                &ctx,
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let conn = ctx.db.lock().unwrap();
        let logs = opencrab_db::queries::list_session_logs_by_session(&conn, "session-1").unwrap();
        let body = logs
            .iter()
            .find(|l| l.log_type == "system")
            .expect("system event");
        let parsed = opencrab_core::context_budget::parse_checkpoint_event(&body.content).unwrap();
        assert_eq!(parsed.position, "waiting");
        assert_eq!(parsed.next, "confirm");
        assert_eq!(parsed.confirmed, vec!["step-1".to_string()]);
    }

    #[tokio::test]
    async fn update_context_checkpoint_oversize_keeps_old() {
        let (_dir, ctx) = test_context();
        let ok = UpdateContextCheckpointAction
            .execute(
                &json!({
                    "confirmed": ["ok"],
                    "position": "here",
                    "next": "there"
                }),
                &ctx,
            )
            .await;
        assert!(ok.success);
        let huge = UpdateContextCheckpointAction
            .execute(
                &json!({
                    "confirmed": ["x".repeat(8_000)],
                    "position": "p",
                    "next": "n"
                }),
                &ctx,
            )
            .await;
        assert!(!huge.success);
        assert_eq!(huge.error.as_deref(), Some("checkpoint_oversize"));
        let conn = ctx.db.lock().unwrap();
        let logs = opencrab_db::queries::list_session_logs_by_session(&conn, "session-1").unwrap();
        let events: Vec<_> = logs
            .iter()
            .filter_map(|l| opencrab_core::context_budget::parse_checkpoint_event(&l.content))
            .collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].position, "here");
    }
}
