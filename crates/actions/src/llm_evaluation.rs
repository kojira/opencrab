use async_trait::async_trait;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

/// 推論結果の自己評価アクション
pub struct EvaluateResponseAction;

#[async_trait]
impl Action for EvaluateResponseAction {
    fn name(&self) -> &str {
        "evaluate_response"
    }

    fn description(&self) -> &str {
        "直前のLLM応答の品質を自己評価し、メトリクスに記録する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["evaluation"],
            "properties": {
                "metrics_id": {
                    "type": "string",
                    "description": "評価対象のメトリクスID（省略時は直前のLLM呼び出しを自動参照）"
                },
                "quality_score": {
                    "type": "number",
                    "description": "品質スコア（0.0-1.0。任意。数値化しにくい場合は省略可）"
                },
                "task_success": {
                    "type": "boolean",
                    "description": "タスクが成功したか"
                },
                "evaluation": {
                    "type": "string",
                    "description": "自由記述の評価。何が良かった/悪かったか、このモデルの特徴、次回への教訓など。"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "自由なタグ（例: ['推論が弱い', 'コスパ良い', '創作向き']）"
                },
                "would_use_again": {
                    "type": "boolean",
                    "description": "同じタスクで同じモデルを使うか"
                },
                "better_model_suggestion": {
                    "type": "string",
                    "description": "より適切だったと思うモデル（自由記述）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let quality_score = args["quality_score"].as_f64().unwrap_or(0.5);
        let task_success = args["task_success"].as_bool().unwrap_or(false);
        let evaluation = args["evaluation"].as_str().unwrap_or("");

        // Meta: record which model is making this evaluation (the evaluator itself).
        let evaluator_model = ctx.model_override.lock()
            .ok()
            .and_then(|m| m.clone());

        // Resolve metrics_id: explicit param or auto-fill from last LLM call.
        let metrics_id = args["metrics_id"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| {
                ctx.last_metrics_id
                    .lock()
                    .ok()
                    .and_then(|id| id.clone())
            });

        // Save tags as JSON string.
        let tags_json = if args["tags"].is_array() {
            Some(serde_json::to_string(&args["tags"]).unwrap_or_default())
        } else {
            None
        };

        if let Some(ref mid) = metrics_id {
            if let Ok(conn) = ctx.db.lock() {
                let _ = opencrab_db::queries::update_llm_metrics_evaluation(
                    &conn,
                    mid,
                    quality_score,
                    task_success,
                    evaluation,
                );
                if let Some(ref tags) = tags_json {
                    let _ = opencrab_db::queries::update_llm_metrics_tags(&conn, mid, tags);
                }
            }
        }

        ActionResult::success(json!({
            "evaluated": true,
            "metrics_id": metrics_id,
            "quality_score": quality_score,
            "task_success": task_success,
            "evaluation": evaluation,
            "tags": tags_json.as_deref().and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok()),
            "meta": {
                "evaluator_model": evaluator_model,
                "note": "この評価はevaluator_modelによって行われた。評価の信頼性はevaluatorの能力に依存する。"
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_context_with_metrics() -> (tempfile::TempDir, ActionContext, String) {
        let conn = opencrab_db::init_memory().unwrap();

        // Insert a metrics record to evaluate.
        let metrics_id = "test-metrics-1".to_string();
        let row = opencrab_db::queries::LlmMetricsRow {
            id: metrics_id.clone(),
            agent_id: "agent-1".to_string(),
            session_id: Some("session-1".to_string()),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            purpose: "conversation".to_string(),
            task_type: None,
            complexity: None,
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            estimated_cost_usd: 0.005,
            latency_ms: 1200,
            time_to_first_token_ms: None,
        };
        opencrab_db::queries::insert_llm_metrics(&conn, &row).unwrap();

        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-1".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: Some("session-1".to_string()),
            db: Arc::new(std::sync::Mutex::new(conn)),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(std::sync::Mutex::new(Some(metrics_id.clone()))),
            model_override: Arc::new(std::sync::Mutex::new(None)),
            current_purpose: Arc::new(std::sync::Mutex::new("conversation".to_string())),
            runtime_info: Arc::new(std::sync::Mutex::new(crate::RuntimeInfo {
                default_model: "mock:test-model".to_string(),
                active_model: None,
                available_providers: vec!["mock".to_string()],
                gateway: "test".to_string(),
            })),
            gateway_admin: None,
        };
        (dir, ctx, metrics_id)
    }

    #[tokio::test]
    async fn test_evaluate_with_explicit_metrics_id() {
        let (_dir, ctx, metrics_id) = test_context_with_metrics();
        let action = EvaluateResponseAction;

        let result = action
            .execute(
                &json!({
                    "metrics_id": metrics_id,
                    "quality_score": 0.9,
                    "task_success": true,
                    "evaluation": "Good response, accurate and helpful",
                }),
                &ctx,
            )
            .await;

        assert!(result.success);
        assert_eq!(result.data.as_ref().unwrap()["evaluated"], true);
        assert_eq!(result.data.as_ref().unwrap()["quality_score"], 0.9);

        // Verify DB was updated.
        let conn = ctx.db.lock().unwrap();
        let (qs, ts, eval): (f64, i32, String) = conn
            .query_row(
                "SELECT quality_score, task_success, self_evaluation FROM llm_usage_metrics WHERE id = ?1",
                rusqlite::params![metrics_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!((qs - 0.9).abs() < 1e-9);
        assert_eq!(ts, 1);
        assert_eq!(eval, "Good response, accurate and helpful");
    }

    #[tokio::test]
    async fn test_evaluate_auto_fills_metrics_id() {
        let (_dir, ctx, metrics_id) = test_context_with_metrics();
        let action = EvaluateResponseAction;

        // Don't pass metrics_id — should auto-fill from last_metrics_id.
        let result = action
            .execute(
                &json!({
                    "quality_score": 0.7,
                    "task_success": false,
                    "evaluation": "Average response",
                }),
                &ctx,
            )
            .await;

        assert!(result.success);
        assert_eq!(
            result.data.as_ref().unwrap()["metrics_id"],
            metrics_id,
        );
    }
}
