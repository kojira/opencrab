//! `search_memory_index`: keyword (and D-1085 §13.3 date range) search over the memory index.

use async_trait::async_trait;
use serde_json::json;

use super::parse_keywords;
use crate::traits::{Action, ActionContext, ActionResult};

/// 記憶インデックスをキーワードで逆引き検索するアクション
pub struct SearchMemoryIndexAction;

#[async_trait]
impl Action for SearchMemoryIndexAction {
    fn name(&self) -> &str {
        "search_memory_index"
    }

    fn description(&self) -> &str {
        "記憶インデックスをキーワードで検索する（タイトル・要約・キーワードの全文検索）。date_from / date_to で期間を絞れる（例: 10月なら date_from=\"2026-10\", date_to=\"2026-10\"）。期間だけ指定すると、その期間のノードを新しい順に返す。ヒットしたノードの short_id を retrieve_memory_nodes に渡すと当時の生ログまで遡れる。生ログ自体を直接検索したい場合は search_my_history を使う。"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "検索キーワード（空白区切りで複数可）。date_from / date_to があれば省略できる"
                },
                "date_from": {
                    "type": "string",
                    "description": "この日以降（YYYY-MM-DD か YYYY-MM。その日・その月を含む）"
                },
                "date_to": {
                    "type": "string",
                    "description": "この日まで（YYYY-MM-DD か YYYY-MM。その日・その月を含む）"
                },
                "limit": {
                    "type": "integer",
                    "description": "最大件数（デフォルト: 10、最大: 25）",
                    "default": 10
                },
                "node_type": {
                    "type": "string",
                    "description": "ノード種別で絞り込む（topic / period / session / unit / category / meta / root。省略時は全種別）"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
        let query = args["query"]
            .as_str()
            .map(str::trim)
            .filter(|q| !q.is_empty());
        let range = match (
            date_bound(&args["date_from"], Bound::Start),
            date_bound(&args["date_to"], Bound::End),
        ) {
            (Ok(from), Ok(to)) => opencrab_db::queries::IndexDateRange { from, to },
            (Err(e), _) | (_, Err(e)) => return ActionResult::error(&e),
        };
        if query.is_none() && range.from.is_none() && range.to.is_none() {
            return ActionResult::error("query or date_from/date_to is required");
        }
        let limit = args["limit"].as_i64().unwrap_or(10).clamp(1, 25) as usize;
        let node_type = args["node_type"].as_str().filter(|s| !s.is_empty());

        let results = {
            let conn = match ctx.db.lock() {
                Ok(c) => c,
                Err(_) => return ActionResult::error("Failed to acquire DB lock"),
            };
            match opencrab_db::queries::search_index_nodes_in_range(
                &conn,
                &ctx.agent_id,
                query,
                limit,
                node_type,
                &range,
            ) {
                Ok(r) => r,
                Err(e) => return ActionResult::error(&format!("Search failed: {e}")),
            }
        };

        let nodes: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                json!({
                    "short_id": r.short_id,
                    "node_id": r.node_id,
                    "node_type": r.node_type,
                    "source_type": r.source_type,
                    "title": r.title,
                    "summary": r.summary,
                    "keywords": parse_keywords(&r.keywords_json),
                    "date_from": r.date_from,
                    "date_to": r.date_to,
                    "child_count": r.child_count,
                    "score": r.score,
                })
            })
            .collect();

        ActionResult::success(json!({
            "query": query,
            "date_from": range.from,
            "date_to": range.to,
            "count": nodes.len(),
            "nodes": nodes,
        }))
    }
}

enum Bound {
    Start,
    End,
}

/// D-1085 §13.3: a `YYYY-MM-DD` or `YYYY-MM` argument as an inclusive day; a month is its first
/// day for [`Bound::Start`] and its last day for [`Bound::End`]. Absent or empty is no bound.
fn date_bound(value: &serde_json::Value, bound: Bound) -> Result<Option<String>, String> {
    let Some(raw) = value.as_str().map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if let Ok(day) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Ok(Some(day.format("%Y-%m-%d").to_string()));
    }
    let first = chrono::NaiveDate::parse_from_str(&format!("{raw}-01"), "%Y-%m-%d")
        .map_err(|_| format!("bad date {raw:?}: use YYYY-MM-DD or YYYY-MM"))?;
    let day = match bound {
        Bound::Start => first,
        Bound::End => first
            .checked_add_months(chrono::Months::new(1))
            .and_then(|next| next.pred_opt())
            .ok_or_else(|| format!("bad date {raw:?}"))?,
    };
    Ok(Some(day.format("%Y-%m-%d").to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::CallerIdentity;

    fn test_context() -> ActionContext {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        std::mem::forget(dir);
        ActionContext {
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
            caller: CallerIdentity::Owner,
        }
    }

    fn seed(ctx: &ActionContext, id: &str, from: &str, to: &str) {
        seed_dates(ctx, id, Some(from), Some(to));
    }

    fn seed_dates(ctx: &ActionContext, id: &str, from: Option<&str>, to: Option<&str>) {
        let conn = ctx.db.lock().unwrap();
        opencrab_db::queries::insert_index_node(
            &conn,
            &opencrab_db::queries::IndexNodeRow {
                id: id.to_string(),
                agent_id: "agent-1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: "session_log".to_string(),
                title: format!("{id} カレー"),
                summary: "s".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: from.map(str::to_string),
                date_to: to.map(str::to_string),
                depth: 3,
                child_count: 0,
                token_count: 0,
                created_at: "2026-11-01T00:00:00Z".to_string(),
                updated_at: "2026-11-01T00:00:00Z".to_string(),
                short_id: Some(id.to_string()),
                keywords_json: "[]".to_string(),
                summary_refreshed_at: None,
            },
        )
        .unwrap();
    }

    fn ids(result: &ActionResult) -> Vec<String> {
        assert!(result.success, "{:?}", result.error);
        result.data.as_ref().unwrap()["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["node_id"].as_str().unwrap().to_string())
            .collect()
    }

    /// D-1085 §13.3: a month (`YYYY-MM`) or day range without a query lists the nodes in it
    /// newest first (a `YYYY-MM` date_to reaches the month's last day, a node with only
    /// date_from is that day, an undated node is never in a range); with a query it narrows the
    /// keyword hits.
    #[tokio::test]
    async fn search_memory_index_by_date_range() {
        let ctx = test_context();
        seed(&ctx, "sep", "2026-09-20", "2026-09-30");
        seed(&ctx, "oct-a", "2026-10-03", "2026-10-03");
        seed(&ctx, "oct-b", "2026-10-18", "2026-10-18");
        seed(&ctx, "oct-end", "2026-10-31", "2026-10-31");
        seed_dates(&ctx, "oct-open", Some("2026-10-25"), None);
        seed(&ctx, "nov", "2026-11-01", "2026-11-01");
        seed_dates(&ctx, "undated", None, None);
        let action = SearchMemoryIndexAction;

        let r = action
            .execute(&json!({"date_from": "2026-10", "date_to": "2026-10"}), &ctx)
            .await;
        assert_eq!(ids(&r), ["oct-end", "oct-open", "oct-b", "oct-a"]);

        let r = action
            .execute(
                &json!({"date_from": "2026-10", "date_to": "2026-10", "limit": 2, "node_type": "topic"}),
                &ctx,
            )
            .await;
        assert_eq!(ids(&r), ["oct-end", "oct-open"]);
        let r = action
            .execute(&json!({"date_to": "2026-10", "node_type": "period"}), &ctx)
            .await;
        assert_eq!(ids(&r), Vec::<String>::new());

        let r = action
            .execute(&json!({"query": "カレー", "date_to": "2026-09-30"}), &ctx)
            .await;
        assert_eq!(ids(&r), ["sep"]);

        let r = action
            .execute(&json!({"query": "カレー", "date_from": "2026-10-20"}), &ctx)
            .await;
        let mut hits = ids(&r);
        hits.sort();
        assert_eq!(hits, ["nov", "oct-end", "oct-open"]);

        assert!(
            !action.execute(&json!({}), &ctx).await.success,
            "query or a date is required"
        );
        assert!(
            !action
                .execute(&json!({"date_from": "10月"}), &ctx)
                .await
                .success,
            "a bad date is an error"
        );
        let required = action.parameters()["required"].clone();
        assert!(
            required.as_array().is_none_or(|r| r.is_empty()),
            "{required}"
        );
    }
}
