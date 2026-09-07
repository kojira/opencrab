use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use crate::traits::{Action, ActionContext, ActionResult};

/// LLM利用状況の生データを返す。判断はエージェントが行う。
pub struct AnalyzeLlmUsageAction;

#[async_trait]
impl Action for AnalyzeLlmUsageAction {
    fn name(&self) -> &str {
        "analyze_llm_usage"
    }

    fn description(&self) -> &str {
        "自分のLLM利用状況の生データを取得する。モデル別・用途別の統計を見て、自分で判断するための材料。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "period": {
                    "type": "string",
                    "description": "分析期間（last_hour, last_day, last_week, last_month, all）"
                },
                "model_filter": {
                    "type": "string",
                    "description": "特定モデルのみ表示（例: gpt-4o）。省略で全モデル。"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let period = args["period"].as_str().unwrap_or("last_week");
        let since = period_to_since(period);

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };

        // Overall summary.
        let summary =
            match opencrab_db::queries::get_llm_metrics_summary(&conn, &ctx.agent_id, &since) {
                Ok(s) => s,
                Err(e) => return ActionResult::error(&format!("Failed: {e}")),
            };

        // Per-model breakdown.
        let model_stats =
            opencrab_db::queries::get_llm_metrics_by_model(&conn, &ctx.agent_id, &since)
                .unwrap_or_default();

        let model_breakdown: Vec<serde_json::Value> = model_stats
            .iter()
            .map(|s| {
                json!({
                    "provider": s.provider,
                    "model": s.model,
                    "requests": s.count,
                    "total_tokens": s.total_tokens,
                    "total_cost_usd": s.total_cost,
                    "avg_latency_ms": s.avg_latency_ms,
                    "avg_quality": s.avg_quality,
                    "success_count": s.success_count,
                })
            })
            .collect();

        // Per-model per-purpose breakdown.
        let purpose_stats = opencrab_db::queries::get_llm_metrics_by_model_and_purpose(
            &conn,
            &ctx.agent_id,
            &since,
        )
        .unwrap_or_default();

        let purpose_breakdown: Vec<serde_json::Value> = purpose_stats
            .iter()
            .map(|s| {
                json!({
                    "provider": s.provider,
                    "model": s.model,
                    "purpose": s.purpose,
                    "requests": s.count,
                    "total_cost_usd": s.total_cost,
                    "avg_latency_ms": s.avg_latency_ms,
                    "avg_quality": s.avg_quality,
                    "success_count": s.success_count,
                })
            })
            .collect();

        ActionResult::success(json!({
            "period": period,
            "summary": {
                "total_requests": summary.count,
                "total_tokens": summary.total_tokens,
                "total_cost_usd": summary.total_cost,
                "avg_latency_ms": summary.avg_latency,
                "avg_quality": summary.avg_quality,
            },
            "by_model": model_breakdown,
            "by_model_and_purpose": purpose_breakdown,
        }))
    }
}

/// 過去のモデル利用経験を思い出す。生データ＋自分の過去の評価コメント＋経験ノートを返す。
/// スコアリングは行わない。判断はエージェント自身が行う。
pub struct RecallModelExperiencesAction;

#[async_trait]
impl Action for RecallModelExperiencesAction {
    fn name(&self) -> &str {
        "recall_model_experiences"
    }

    fn description(&self) -> &str {
        "過去のモデル利用経験を思い出す。数値メトリクス、自分の評価コメント、経験ノートをまとめて返す。どのモデルを使うかは自分で判断する。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "model_filter": {
                    "type": "string",
                    "description": "特定モデルに絞り込み（例: gpt-4o）。省略で全モデル。"
                },
                "include_notes": {
                    "type": "boolean",
                    "description": "経験ノートを含めるか（デフォルト: true）"
                },
                "evaluation_limit": {
                    "type": "integer",
                    "description": "取得する過去評価の件数上限（デフォルト: 20）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let model_filter = args["model_filter"].as_str();
        let include_notes = args["include_notes"].as_bool().unwrap_or(true);
        let eval_limit = args["evaluation_limit"].as_u64().unwrap_or(20) as usize;

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };

        // 1. Raw metrics by model.
        let model_stats = opencrab_db::queries::get_llm_metrics_by_model(
            &conn,
            &ctx.agent_id,
            "1970-01-01T00:00:00Z",
        )
        .unwrap_or_default();

        let metrics: Vec<serde_json::Value> = model_stats
            .iter()
            .filter(|s| model_filter.is_none_or(|f| s.model == f))
            .map(|s| {
                json!({
                    "provider": s.provider,
                    "model": s.model,
                    "total_requests": s.count,
                    "total_cost_usd": s.total_cost,
                    "avg_latency_ms": s.avg_latency_ms,
                    "avg_quality": s.avg_quality,
                    "success_count": s.success_count,
                })
            })
            .collect();

        // 2. Per-purpose breakdown.
        let purpose_stats = opencrab_db::queries::get_llm_metrics_by_model_and_purpose(
            &conn,
            &ctx.agent_id,
            "1970-01-01T00:00:00Z",
        )
        .unwrap_or_default();

        let by_purpose: Vec<serde_json::Value> = purpose_stats
            .iter()
            .filter(|s| model_filter.is_none_or(|f| s.model == f))
            .map(|s| {
                json!({
                    "provider": s.provider,
                    "model": s.model,
                    "purpose": s.purpose,
                    "requests": s.count,
                    "total_cost_usd": s.total_cost,
                    "avg_latency_ms": s.avg_latency_ms,
                    "avg_quality": s.avg_quality,
                })
            })
            .collect();

        // 3. Recent evaluations (with free-text feedback).
        let evaluations = opencrab_db::queries::get_recent_evaluations(
            &conn,
            &ctx.agent_id,
            model_filter,
            eval_limit,
        )
        .unwrap_or_default();

        let eval_entries: Vec<serde_json::Value> = evaluations
            .iter()
            .map(|(model, purpose, eval_text, quality, tags, timestamp)| {
                json!({
                    "model": model,
                    "purpose": purpose,
                    "evaluation": eval_text,
                    "quality_score": quality,
                    "tags": tags.as_deref().and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok()),
                    "timestamp": timestamp,
                })
            })
            .collect();

        // 4. Experience notes.
        let notes = if include_notes {
            let notes = opencrab_db::queries::list_model_experience_notes(
                &conn,
                &ctx.agent_id,
                model_filter,
            )
            .unwrap_or_default();

            let note_entries: Vec<serde_json::Value> = notes
                .iter()
                .map(|n| {
                    json!({
                        "id": n.id,
                        "provider": n.provider,
                        "model": n.model,
                        "situation": n.situation,
                        "observation": n.observation,
                        "recommendation": n.recommendation,
                        "tags": n.tags.as_deref().and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok()),
                        "created_at": n.created_at,
                    })
                })
                .collect();
            Some(note_entries)
        } else {
            None
        };

        let mut result = json!({
            "model_metrics": metrics,
            "by_purpose": by_purpose,
            "recent_evaluations": eval_entries,
        });

        if let Some(notes) = notes {
            result["experience_notes"] = json!(notes);
        }

        ActionResult::success(result)
    }
}

/// モデル利用の経験ノートを保存する。定性的な知見を自由に記録できる。
pub struct SaveModelInsightAction;

#[async_trait]
impl Action for SaveModelInsightAction {
    fn name(&self) -> &str {
        "save_model_insight"
    }

    fn description(&self) -> &str {
        "モデル利用で得た知見を記録する。定量データでは表せない経験的な観察、推薦、注意点を自由に書ける。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["situation", "observation"],
            "properties": {
                "provider": {
                    "type": "string",
                    "description": "対象プロバイダー（例: openai, anthropic）。省略可（一般的な知見の場合）。"
                },
                "model": {
                    "type": "string",
                    "description": "対象モデル（例: gpt-4o）。省略可（一般的な知見の場合）。"
                },
                "situation": {
                    "type": "string",
                    "description": "どんな場面・タスクでの経験か（自由記述）"
                },
                "observation": {
                    "type": "string",
                    "description": "何が起きたか、どう感じたか（自由記述）"
                },
                "recommendation": {
                    "type": "string",
                    "description": "次回同じ場面でどうすべきか（自由記述）"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "自由なタグ（例: ['complex-reasoning', 'cost-sensitive', 'fast-response-needed']）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let situation = match args["situation"].as_str() {
            Some(s) if !s.is_empty() => s,
            _ => return ActionResult::error("situation is required"),
        };
        let observation = match args["observation"].as_str() {
            Some(s) if !s.is_empty() => s,
            _ => return ActionResult::error("observation is required"),
        };

        // Meta: which model is writing this insight?
        let author_model = ctx.model_override.lock().ok().and_then(|m| m.clone());

        let provider = args["provider"].as_str();
        let model = args["model"].as_str();
        let recommendation = args["recommendation"].as_str();

        // Merge user tags with auto-generated meta tag.
        let mut tag_list: Vec<serde_json::Value> = if args["tags"].is_array() {
            args["tags"].as_array().unwrap().clone()
        } else {
            vec![]
        };
        if let Some(ref am) = author_model {
            tag_list.push(json!(format!("authored_by:{am}")));
        }
        let tags = if tag_list.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&tag_list).unwrap_or_default())
        };

        let note_id = uuid::Uuid::new_v4().to_string();
        let note = opencrab_db::queries::ModelExperienceNote {
            id: note_id.clone(),
            agent_id: ctx.agent_id.clone(),
            provider: provider.map(|s| s.to_string()),
            model: model.map(|s| s.to_string()),
            situation: situation.to_string(),
            observation: observation.to_string(),
            recommendation: recommendation.map(|s| s.to_string()),
            tags,
            created_at: None,
        };

        let conn = match ctx.db.lock() {
            Ok(c) => c,
            Err(_) => return ActionResult::error("Failed to acquire DB lock"),
        };

        match opencrab_db::queries::insert_model_experience_note(&conn, &note) {
            Ok(_) => ActionResult::success(json!({
                "saved": true,
                "note_id": note_id,
                "model": model,
                "situation": situation,
                "meta": {
                    "author_model": author_model,
                    "note": "この知見はauthor_modelによって記録された。"
                }
            })),
            Err(e) => ActionResult::error(&format!("Failed to save: {e}")),
        }
    }
}

fn period_to_since(period: &str) -> String {
    match period {
        "last_hour" => (Utc::now() - chrono::Duration::hours(1)).to_rfc3339(),
        "last_day" => (Utc::now() - chrono::Duration::days(1)).to_rfc3339(),
        "last_week" => (Utc::now() - chrono::Duration::weeks(1)).to_rfc3339(),
        "last_month" => (Utc::now() - chrono::Duration::days(30)).to_rfc3339(),
        _ => "1970-01-01T00:00:00Z".to_string(),
    }
}

#[cfg(test)]
mod tests;
