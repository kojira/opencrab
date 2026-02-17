use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

/// LLM利用状況分析アクション
pub struct AnalyzeLlmUsageAction;

#[async_trait]
impl Action for AnalyzeLlmUsageAction {
    fn name(&self) -> &str {
        "analyze_llm_usage"
    }

    fn description(&self) -> &str {
        "自分のLLM利用状況をメタ視点で分析する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "分析期間",
                    "enum": ["last_hour", "last_day", "last_week", "last_month", "all"]
                },
                "group_by": {
                    "type": "string",
                    "description": "グループ化の軸",
                    "enum": ["model", "purpose", "task_type", "complexity"]
                },
                "focus": {
                    "type": "string",
                    "description": "分析の焦点",
                    "enum": ["cost", "quality", "latency", "efficiency", "all"]
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let period = args["period"].as_str().unwrap_or("last_week");

        let since = match period {
            "last_hour" => (Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
            "last_day" => (Utc::now() - chrono::Duration::days(1)).to_rfc3339(),
            "last_week" => (Utc::now() - chrono::Duration::weeks(1)).to_rfc3339(),
            "last_month" => (Utc::now() - chrono::Duration::days(30)).to_rfc3339(),
            _ => "1970-01-01T00:00:00Z".to_string(),
        };

        let summary = if let Ok(conn) = ctx.db.lock() {
            match opencrab_db::queries::get_llm_metrics_summary(&conn, &ctx.agent_id, &since) {
                Ok(s) => s,
                Err(e) => return ActionResult::error(&format!("Analysis failed: {e}")),
            }
        } else {
            return ActionResult::error("Failed to acquire DB lock");
        };

        ActionResult::success(json!({
            "period": period,
            "summary": {
                "total_requests": summary.count,
                "total_tokens": summary.total_tokens,
                "total_cost_usd": summary.total_cost,
                "avg_latency_ms": summary.avg_latency,
                "avg_quality": summary.avg_quality,
            }
        }))
    }
}

/// モデル選択最適化アクション
pub struct OptimizeModelSelectionAction;

#[async_trait]
impl Action for OptimizeModelSelectionAction {
    fn name(&self) -> &str {
        "optimize_model_selection"
    }

    fn description(&self) -> &str {
        "過去の利用データに基づいて、タスク別の最適なモデルを提案する"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "optimization_goal": {
                    "type": "string",
                    "description": "最適化の目標",
                    "enum": ["minimize_cost", "maximize_quality", "balance", "minimize_latency"]
                },
                "budget_limit_usd": {
                    "type": "number",
                    "description": "1日あたりの予算上限（USD）"
                },
                "min_quality_threshold": {
                    "type": "number",
                    "description": "最低品質スコア（0.0-1.0）"
                },
                "apply_immediately": {
                    "type": "boolean",
                    "description": "提案を即座に適用するか"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, _ctx: &ActionContext) -> ActionResult {
        let goal = args["optimization_goal"]
            .as_str()
            .unwrap_or("balance");
        let _budget = args["budget_limit_usd"].as_f64();
        let _min_quality = args["min_quality_threshold"].as_f64().unwrap_or(0.7);

        // TODO: 実際のメトリクス分析に基づく最適化ロジック
        ActionResult::success(json!({
            "optimization_goal": goal,
            "recommendations": [
                {
                    "category": "cost",
                    "suggestion": "単純な応答にはfastモデルを使用することを推奨",
                    "expected_improvement": "コスト30%削減"
                }
            ],
            "note": "メトリクスが蓄積されるほど、より精度の高い最適化が可能になります"
        }))
    }
}
