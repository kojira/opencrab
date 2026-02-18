use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct AnalyticsQuery {
    pub period: Option<String>,
}

fn period_to_since(period: &str) -> String {
    let duration = match period {
        "day" => chrono::Duration::days(1),
        "week" => chrono::Duration::weeks(1),
        "month" => chrono::Duration::days(30),
        _ => chrono::Duration::weeks(1),
    };
    let since = chrono::Utc::now() - duration;
    since.to_rfc3339()
}

pub async fn get_metrics_summary(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<AnalyticsQuery>,
) -> Json<serde_json::Value> {
    let period = query.period.as_deref().unwrap_or("week");
    let since = period_to_since(period);
    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::get_llm_metrics_summary(&conn, &id, &since) {
        Ok(summary) => Json(serde_json::json!({
            "count": summary.count,
            "total_tokens": summary.total_tokens.unwrap_or(0),
            "total_cost": summary.total_cost.unwrap_or(0.0),
            "avg_latency": summary.avg_latency.unwrap_or(0.0),
            "avg_quality": summary.avg_quality.unwrap_or(0.0),
        })),
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}

pub async fn get_metrics_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<AnalyticsQuery>,
) -> Json<serde_json::Value> {
    let period = query.period.as_deref().unwrap_or("week");
    let since = period_to_since(period);
    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::get_llm_metrics_by_model(&conn, &id, &since) {
        Ok(models) => {
            let data: Vec<serde_json::Value> = models
                .into_iter()
                .map(|m| {
                    serde_json::json!({
                        "provider": m.provider,
                        "model": m.model,
                        "total_tokens": m.total_tokens,
                        "total_cost": m.total_cost,
                        "request_count": m.count,
                        "avg_latency": m.avg_latency_ms,
                    })
                })
                .collect();
            Json(serde_json::json!(data))
        }
        Err(e) => Json(serde_json::json!({"error": e.to_string()})),
    }
}
