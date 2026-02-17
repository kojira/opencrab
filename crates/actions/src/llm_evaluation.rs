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
            "required": ["quality_score", "task_success", "evaluation"],
            "properties": {
                "metrics_id": {
                    "type": "string",
                    "description": "評価対象のメトリクスID"
                },
                "quality_score": {
                    "type": "number",
                    "description": "品質スコア（0.0-1.0）",
                    "minimum": 0.0,
                    "maximum": 1.0
                },
                "task_success": {
                    "type": "boolean",
                    "description": "タスクが成功したか"
                },
                "evaluation": {
                    "type": "string",
                    "description": "評価コメント（何が良かった/悪かったか）"
                },
                "would_use_again": {
                    "type": "boolean",
                    "description": "同じタスクで同じモデルを使うか"
                },
                "better_model_suggestion": {
                    "type": "string",
                    "description": "より適切だったと思うモデル（オプション）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let quality_score = args["quality_score"].as_f64().unwrap_or(0.5);
        let task_success = args["task_success"].as_bool().unwrap_or(false);
        let evaluation = args["evaluation"].as_str().unwrap_or("");

        // メトリクスIDが指定されていれば、そのメトリクスを更新
        if let Some(metrics_id) = args["metrics_id"].as_str() {
            if let Ok(conn) = ctx.db.lock() {
                let _ = opencrab_db::queries::update_llm_metrics_evaluation(
                    &conn,
                    metrics_id,
                    quality_score,
                    task_success,
                    evaluation,
                );
            }
        }

        ActionResult::success(json!({
            "evaluated": true,
            "quality_score": quality_score,
            "task_success": task_success,
            "evaluation": evaluation,
        }))
    }
}
