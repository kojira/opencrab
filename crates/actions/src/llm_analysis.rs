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
        let summary = match opencrab_db::queries::get_llm_metrics_summary(&conn, &ctx.agent_id, &since) {
            Ok(s) => s,
            Err(e) => return ActionResult::error(&format!("Failed: {e}")),
        };

        // Per-model breakdown.
        let model_stats = opencrab_db::queries::get_llm_metrics_by_model(&conn, &ctx.agent_id, &since)
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
            &conn, &ctx.agent_id, &since,
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
            &conn, &ctx.agent_id, "1970-01-01T00:00:00Z",
        )
        .unwrap_or_default();

        let metrics: Vec<serde_json::Value> = model_stats
            .iter()
            .filter(|s| model_filter.map_or(true, |f| s.model == f))
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
            &conn, &ctx.agent_id, "1970-01-01T00:00:00Z",
        )
        .unwrap_or_default();

        let by_purpose: Vec<serde_json::Value> = purpose_stats
            .iter()
            .filter(|s| model_filter.map_or(true, |f| s.model == f))
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
            &conn, &ctx.agent_id, model_filter, eval_limit,
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
                &conn, &ctx.agent_id, model_filter,
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
        let author_model = ctx.model_override.lock()
            .ok()
            .and_then(|m| m.clone());

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
mod tests {
    use super::*;
    use std::sync::Arc;

    fn seed_diverse_metrics(conn: &rusqlite::Connection) {
        let scenarios = vec![
            // (id_prefix, provider, model, purpose, cost, latency, quality, eval_text, count)
            ("conv-4o",    "openai",    "gpt-4o",       "会話",     0.0030, 2000, 0.90, "丁寧で正確な回答", 3),
            ("conv-mini",  "openai",    "gpt-4o-mini",  "会話",     0.0003, 400,  0.82, "速いが浅い回答", 5),
            ("conv-claude","anthropic", "claude-sonnet-4", "会話",     0.0015, 1200, 0.88, "自然な文体だった", 4),
            ("anl-4o",     "openai",    "gpt-4o",       "複雑な推論", 0.0050, 3000, 0.95, "難しい問題も正確に解けた", 4),
            ("anl-mini",   "openai",    "gpt-4o-mini",  "複雑な推論", 0.0005, 600,  0.45, "推論が浅く間違いが多かった", 2),
            ("anl-claude", "anthropic", "claude-sonnet-4", "複雑な推論", 0.0025, 1500, 0.93, "論理的で良かった", 5),
            ("cre-claude", "anthropic", "claude-sonnet-4", "創作",     0.0020, 1300, 0.94, "独創的な表現が良かった", 4),
            ("cre-mini",   "openai",    "gpt-4o-mini",  "創作",     0.0004, 500,  0.65, "テンプレ的でつまらない", 2),
            ("tc-mini",    "openai",    "gpt-4o-mini",  "ツール呼び出し", 0.0003, 350, 0.85, "tool_callsの精度が高い", 6),
            ("tc-4o",      "openai",    "gpt-4o",       "ツール呼び出し", 0.0035, 1800, 0.88, "正確だがコスト高", 3),
        ];

        for (prefix, provider, model, purpose, cost, latency, quality, eval, count) in scenarios {
            for i in 0..count {
                let id = format!("{prefix}-{i}");
                let row = opencrab_db::queries::LlmMetricsRow {
                    id: id.clone(),
                    agent_id: "agent-1".to_string(),
                    session_id: Some("s-1".to_string()),
                    timestamp: "2024-01-01T00:00:00Z".to_string(),
                    provider: provider.to_string(),
                    model: model.to_string(),
                    purpose: purpose.to_string(),
                    task_type: None,
                    complexity: None,
                    input_tokens: 300,
                    output_tokens: 150,
                    total_tokens: 450,
                    estimated_cost_usd: cost,
                    latency_ms: latency,
                    time_to_first_token_ms: None,
                };
                opencrab_db::queries::insert_llm_metrics(conn, &row).unwrap();
                opencrab_db::queries::update_llm_metrics_evaluation(
                    conn, &id, quality, quality >= 0.7, eval,
                ).unwrap();
            }
        }
    }

    fn seed_experience_notes(conn: &rusqlite::Connection) {
        let notes = vec![
            ("gpt-4o", "openai", "複雑な数学の証明を頼んだとき", "ステップバイステップで正確に解けた。コストは高いが複雑なタスクでは価値がある。", Some("複雑な推論タスクではgpt-4oを使うべき"), r#"["複雑な推論","数学","高品質"]"#),
            ("gpt-4o-mini", "openai", "簡単な質問応答", "十分な品質で非常に高速。コスト効率が良い。", Some("簡単なタスクやツール呼び出しにはgpt-4o-miniで十分"), r#"["簡単なタスク","コスト重視","高速"]"#),
            ("gpt-4o-mini", "openai", "複雑な分析タスクを頼んだとき", "推論が浅く、重要なポイントを見落とした。この種のタスクには向かない。", Some("複雑な分析にはgpt-4o-miniを使わない"), r#"["複雑な推論","失敗","要注意"]"#),
            ("claude-sonnet-4", "anthropic", "創作文を書かせたとき", "独創的で文学的な表現ができた。創作タスクでは最も良い結果。", Some("創作タスクではclaude-sonnetがベスト"), r#"["創作","高品質","推薦"]"#),
        ];

        for (model, provider, situation, observation, recommendation, tags) in notes {
            let note = opencrab_db::queries::ModelExperienceNote {
                id: uuid::Uuid::new_v4().to_string(),
                agent_id: "agent-1".to_string(),
                provider: Some(provider.to_string()),
                model: Some(model.to_string()),
                situation: situation.to_string(),
                observation: observation.to_string(),
                recommendation: recommendation.map(|s| s.to_string()),
                tags: Some(tags.to_string()),
                created_at: None,
            };
            opencrab_db::queries::insert_model_experience_note(conn, &note).unwrap();
        }
    }

    fn test_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        seed_diverse_metrics(&conn);
        seed_experience_notes(&conn);

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
            gateway_admin: None,
        };
        (dir, ctx)
    }

    fn empty_context() -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "agent-empty".to_string(),
            agent_name: "Empty".to_string(),
            session_id: None,
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
            gateway_admin: None,
        };
        (dir, ctx)
    }

    // ── AnalyzeLlmUsageAction ──

    #[tokio::test]
    async fn test_analyze_returns_raw_data() {
        let (_dir, ctx) = test_context();
        let action = AnalyzeLlmUsageAction;

        let result = action
            .execute(&json!({"period": "all"}), &ctx)
            .await;

        assert!(result.success);
        let data = result.data.unwrap();

        let total = data["summary"]["total_requests"].as_i64().unwrap();
        assert!(total > 0);

        // by_model has 3 models.
        let by_model = data["by_model"].as_array().unwrap();
        assert_eq!(by_model.len(), 3);

        // by_model_and_purpose has entries with free-form purposes.
        let by_mp = data["by_model_and_purpose"].as_array().unwrap();
        assert!(!by_mp.is_empty());

        // Purposes are free-form Japanese text, not enum.
        let purposes: Vec<&str> = by_mp.iter()
            .filter_map(|e| e["purpose"].as_str())
            .collect();
        assert!(purposes.contains(&"会話"));
        assert!(purposes.contains(&"複雑な推論"));
        assert!(purposes.contains(&"創作"));
    }

    #[tokio::test]
    async fn test_analyze_empty() {
        let (_dir, ctx) = empty_context();
        let action = AnalyzeLlmUsageAction;

        let result = action.execute(&json!({"period": "all"}), &ctx).await;
        assert!(result.success);
        assert_eq!(result.data.unwrap()["summary"]["total_requests"], 0);
    }

    // ── RecallModelExperiencesAction ──

    #[tokio::test]
    async fn test_recall_returns_metrics_evaluations_and_notes() {
        let (_dir, ctx) = test_context();
        let action = RecallModelExperiencesAction;

        let result = action.execute(&json!({}), &ctx).await;
        assert!(result.success);
        let data = result.data.unwrap();

        // Has model metrics.
        let metrics = data["model_metrics"].as_array().unwrap();
        assert_eq!(metrics.len(), 3);

        // Has per-purpose breakdown.
        let by_purpose = data["by_purpose"].as_array().unwrap();
        assert!(!by_purpose.is_empty());

        // Has recent evaluations with free text.
        let evals = data["recent_evaluations"].as_array().unwrap();
        assert!(!evals.is_empty());
        // Evaluations contain free-text Japanese.
        let has_japanese_eval = evals.iter().any(|e|
            e["evaluation"].as_str().map_or(false, |t| t.contains("丁寧"))
        );
        assert!(has_japanese_eval, "Evaluations should contain free-text feedback");

        // Has experience notes.
        let notes = data["experience_notes"].as_array().unwrap();
        assert_eq!(notes.len(), 4);
        // Notes contain qualitative observations.
        let has_insight = notes.iter().any(|n|
            n["observation"].as_str().map_or(false, |t| t.contains("ステップバイステップ"))
        );
        assert!(has_insight, "Notes should contain qualitative observations");

        // Print what the agent would see.
        eprintln!("\n=== What the agent sees (recall_model_experiences) ===\n");
        eprintln!("--- Model Metrics ---");
        for m in metrics {
            eprintln!(
                "  {:16} {} requests, ${:.4} total, {:.0}ms avg, quality={:.2}",
                m["model"].as_str().unwrap(),
                m["total_requests"],
                m["total_cost_usd"].as_f64().unwrap(),
                m["avg_latency_ms"].as_f64().unwrap(),
                m["avg_quality"].as_f64().unwrap_or(0.0),
            );
        }
        eprintln!("\n--- Recent Evaluations (free text) ---");
        for e in evals.iter().take(5) {
            eprintln!(
                "  [{}] {} (quality={:.2}): {}",
                e["purpose"].as_str().unwrap_or("?"),
                e["model"].as_str().unwrap(),
                e["quality_score"].as_f64().unwrap(),
                e["evaluation"].as_str().unwrap(),
            );
        }
        eprintln!("\n--- Experience Notes ---");
        for n in notes {
            eprintln!(
                "  [{}] {}: {} → {}",
                n["model"].as_str().unwrap_or("general"),
                n["situation"].as_str().unwrap(),
                n["observation"].as_str().unwrap(),
                n["recommendation"].as_str().unwrap_or("(no recommendation)"),
            );
        }
    }

    #[tokio::test]
    async fn test_recall_with_model_filter() {
        let (_dir, ctx) = test_context();
        let action = RecallModelExperiencesAction;

        let result = action.execute(&json!({"model_filter": "gpt-4o-mini"}), &ctx).await;
        assert!(result.success);
        let data = result.data.unwrap();

        // Metrics should only show gpt-4o-mini.
        let metrics = data["model_metrics"].as_array().unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0]["model"], "gpt-4o-mini");

        // Notes should only show gpt-4o-mini notes (2 notes).
        let notes = data["experience_notes"].as_array().unwrap();
        assert_eq!(notes.len(), 2);
    }

    #[tokio::test]
    async fn test_recall_empty() {
        let (_dir, ctx) = empty_context();
        let action = RecallModelExperiencesAction;

        let result = action.execute(&json!({}), &ctx).await;
        assert!(result.success);
        let data = result.data.unwrap();
        assert!(data["model_metrics"].as_array().unwrap().is_empty());
        assert!(data["experience_notes"].as_array().unwrap().is_empty());
    }

    // ── SaveModelInsightAction ──

    #[tokio::test]
    async fn test_save_insight() {
        let (_dir, ctx) = test_context();
        let action = SaveModelInsightAction;

        let result = action.execute(
            &json!({
                "provider": "openai",
                "model": "gpt-4o",
                "situation": "長文の要約タスク",
                "observation": "要点を正確に抽出できた。ただしコストが高い。",
                "recommendation": "要約タスクではgpt-4oが最適だが、短い文章ならgpt-4o-miniでも十分かもしれない",
                "tags": ["要約", "コスト検討", "高品質"]
            }),
            &ctx,
        ).await;

        assert!(result.success);
        let data = result.data.unwrap();
        assert_eq!(data["saved"], true);
        assert!(data["note_id"].is_string());

        // Verify it's stored in DB.
        let conn = ctx.db.lock().unwrap();
        let notes = opencrab_db::queries::list_model_experience_notes(&conn, "agent-1", Some("gpt-4o")).unwrap();
        let has_new_note = notes.iter().any(|n| n.situation.contains("長文の要約"));
        assert!(has_new_note);
    }

    #[tokio::test]
    async fn test_save_insight_general() {
        let (_dir, ctx) = test_context();
        let action = SaveModelInsightAction;

        // General insight without specific model.
        let result = action.execute(
            &json!({
                "situation": "コストが予算を超えそうなとき",
                "observation": "安いモデルに切り替えてもタスクの種類によっては品質が大幅に下がる",
                "tags": ["コスト", "一般知見"]
            }),
            &ctx,
        ).await;

        assert!(result.success);
    }

    #[tokio::test]
    async fn test_save_insight_requires_situation() {
        let (_dir, ctx) = test_context();
        let action = SaveModelInsightAction;

        let result = action.execute(
            &json!({"observation": "test"}),
            &ctx,
        ).await;

        assert!(!result.success);
    }
}
