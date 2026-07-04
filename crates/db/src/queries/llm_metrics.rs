use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// LLM Metrics
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMetricsRow {
    pub id: String,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub timestamp: String,
    pub provider: String,
    pub model: String,
    pub purpose: String,
    pub task_type: Option<String>,
    pub complexity: Option<String>,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub total_tokens: i32,
    pub estimated_cost_usd: f64,
    pub latency_ms: i64,
    pub time_to_first_token_ms: Option<i64>,
}

pub fn insert_llm_metrics(conn: &Connection, metrics: &LlmMetricsRow) -> Result<()> {
    conn.execute(
        "INSERT INTO llm_usage_metrics (id, agent_id, session_id, timestamp, provider, model, purpose, task_type, complexity, input_tokens, output_tokens, total_tokens, estimated_cost_usd, latency_ms, time_to_first_token_ms, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            metrics.id,
            metrics.agent_id,
            metrics.session_id,
            metrics.timestamp,
            metrics.provider,
            metrics.model,
            metrics.purpose,
            metrics.task_type,
            metrics.complexity,
            metrics.input_tokens,
            metrics.output_tokens,
            metrics.total_tokens,
            metrics.estimated_cost_usd,
            metrics.latency_ms,
            metrics.time_to_first_token_ms,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn update_llm_metrics_evaluation(
    conn: &Connection,
    metrics_id: &str,
    quality_score: f64,
    task_success: bool,
    self_evaluation: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE llm_usage_metrics SET quality_score = ?1, task_success = ?2, self_evaluation = ?3 WHERE id = ?4",
        params![quality_score, task_success as i32, self_evaluation, metrics_id],
    )?;
    Ok(())
}

pub fn update_llm_metrics_tags(conn: &Connection, metrics_id: &str, tags_json: &str) -> Result<()> {
    conn.execute(
        "UPDATE llm_usage_metrics SET tags = ?1 WHERE id = ?2",
        params![tags_json, metrics_id],
    )?;
    Ok(())
}

// ============================================
// Model Experience Notes
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelExperienceNote {
    pub id: String,
    pub agent_id: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub situation: String,
    pub observation: String,
    pub recommendation: Option<String>,
    pub tags: Option<String>,
    pub created_at: Option<String>,
}

pub fn insert_model_experience_note(conn: &Connection, note: &ModelExperienceNote) -> Result<()> {
    conn.execute(
        "INSERT INTO model_experience_notes (id, agent_id, provider, model, situation, observation, recommendation, tags, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            note.id,
            note.agent_id,
            note.provider,
            note.model,
            note.situation,
            note.observation,
            note.recommendation,
            note.tags,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn list_model_experience_notes(
    conn: &Connection,
    agent_id: &str,
    model_filter: Option<&str>,
) -> Result<Vec<ModelExperienceNote>> {
    let (sql, param_values): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(model) =
        model_filter
    {
        (
            "SELECT id, agent_id, provider, model, situation, observation, recommendation, tags, created_at
             FROM model_experience_notes WHERE agent_id = ?1 AND model = ?2 ORDER BY created_at DESC",
            vec![Box::new(agent_id.to_string()), Box::new(model.to_string())],
        )
    } else {
        (
            "SELECT id, agent_id, provider, model, situation, observation, recommendation, tags, created_at
             FROM model_experience_notes WHERE agent_id = ?1 ORDER BY created_at DESC",
            vec![Box::new(agent_id.to_string())],
        )
    };

    let mut stmt = conn.prepare(sql)?;
    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok(ModelExperienceNote {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            provider: row.get(2)?,
            model: row.get(3)?,
            situation: row.get(4)?,
            observation: row.get(5)?,
            recommendation: row.get(6)?,
            tags: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Get recent evaluations with free-text feedback (self_evaluation) for a model.
pub fn get_recent_evaluations(
    conn: &Connection,
    agent_id: &str,
    model_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, String, String, f64, Option<String>, Option<String>)>> {
    // Returns: (model, purpose, self_evaluation, quality_score, tags, timestamp)
    let (sql, param_values): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(model) =
        model_filter
    {
        (
            "SELECT model, purpose, COALESCE(self_evaluation, ''), COALESCE(quality_score, 0.0), tags, timestamp
             FROM llm_usage_metrics
             WHERE agent_id = ?1 AND model = ?2 AND self_evaluation IS NOT NULL
             ORDER BY timestamp DESC LIMIT ?3",
            vec![
                Box::new(agent_id.to_string()),
                Box::new(model.to_string()),
                Box::new(limit as i64),
            ],
        )
    } else {
        (
            "SELECT model, purpose, COALESCE(self_evaluation, ''), COALESCE(quality_score, 0.0), tags, timestamp
             FROM llm_usage_metrics
             WHERE agent_id = ?1 AND self_evaluation IS NOT NULL
             ORDER BY timestamp DESC LIMIT ?2",
            vec![
                Box::new(agent_id.to_string()),
                Box::new(limit as i64),
            ],
        )
    };

    let mut stmt = conn.prepare(sql)?;
    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, f64>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMetricsSummary {
    pub count: i64,
    pub total_tokens: Option<i64>,
    pub total_cost: Option<f64>,
    pub avg_latency: Option<f64>,
    pub avg_quality: Option<f64>,
}

pub fn get_llm_metrics_summary(
    conn: &Connection,
    agent_id: &str,
    since: &str,
) -> Result<LlmMetricsSummary> {
    let row = conn.query_row(
        "SELECT
            COUNT(*) as count,
            SUM(total_tokens) as total_tokens,
            SUM(estimated_cost_usd) as total_cost,
            AVG(latency_ms) as avg_latency,
            AVG(quality_score) as avg_quality
         FROM llm_usage_metrics
         WHERE agent_id = ?1 AND timestamp >= ?2",
        params![agent_id, since],
        |row| {
            Ok(LlmMetricsSummary {
                count: row.get(0)?,
                total_tokens: row.get(1)?,
                total_cost: row.get(2)?,
                avg_latency: row.get(3)?,
                avg_quality: row.get(4)?,
            })
        },
    )?;

    Ok(row)
}

/// Per-model aggregated metrics for optimization analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmModelStats {
    pub provider: String,
    pub model: String,
    pub count: i64,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub avg_latency_ms: f64,
    pub avg_quality: Option<f64>,
    pub success_count: i64,
}

/// Get per-model aggregated metrics for an agent since a given timestamp.
pub fn get_llm_metrics_by_model(
    conn: &Connection,
    agent_id: &str,
    since: &str,
) -> Result<Vec<LlmModelStats>> {
    let mut stmt = conn.prepare(
        "SELECT
            provider,
            model,
            COUNT(*) as count,
            COALESCE(SUM(total_tokens), 0) as total_tokens,
            COALESCE(SUM(estimated_cost_usd), 0.0) as total_cost,
            COALESCE(AVG(latency_ms), 0.0) as avg_latency_ms,
            AVG(quality_score) as avg_quality,
            COALESCE(SUM(CASE WHEN task_success = 1 THEN 1 ELSE 0 END), 0) as success_count
         FROM llm_usage_metrics
         WHERE agent_id = ?1 AND timestamp >= ?2
         GROUP BY provider, model
         ORDER BY count DESC",
    )?;

    let rows = stmt.query_map(params![agent_id, since], |row| {
        Ok(LlmModelStats {
            provider: row.get(0)?,
            model: row.get(1)?,
            count: row.get(2)?,
            total_tokens: row.get(3)?,
            total_cost: row.get(4)?,
            avg_latency_ms: row.get(5)?,
            avg_quality: row.get(6)?,
            success_count: row.get(7)?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// Per-model per-purpose aggregated stats for scenario-based optimization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmModelPurposeStats {
    pub provider: String,
    pub model: String,
    pub purpose: String,
    pub count: i64,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub avg_latency_ms: f64,
    pub avg_quality: Option<f64>,
    pub success_count: i64,
}

/// Get per-model per-purpose aggregated metrics for scenario-based optimization.
/// Groups by (provider, model, purpose) to enable "use model X for analysis, model Y for chat".
pub fn get_llm_metrics_by_model_and_purpose(
    conn: &Connection,
    agent_id: &str,
    since: &str,
) -> Result<Vec<LlmModelPurposeStats>> {
    let mut stmt = conn.prepare(
        "SELECT
            provider,
            model,
            purpose,
            COUNT(*) as count,
            COALESCE(SUM(total_tokens), 0) as total_tokens,
            COALESCE(SUM(estimated_cost_usd), 0.0) as total_cost,
            COALESCE(AVG(latency_ms), 0.0) as avg_latency_ms,
            AVG(quality_score) as avg_quality,
            COALESCE(SUM(CASE WHEN task_success = 1 THEN 1 ELSE 0 END), 0) as success_count
         FROM llm_usage_metrics
         WHERE agent_id = ?1 AND timestamp >= ?2
         GROUP BY provider, model, purpose
         ORDER BY purpose, count DESC",
    )?;

    let rows = stmt.query_map(params![agent_id, since], |row| {
        Ok(LlmModelPurposeStats {
            provider: row.get(0)?,
            model: row.get(1)?,
            purpose: row.get(2)?,
            count: row.get(3)?,
            total_tokens: row.get(4)?,
            total_cost: row.get(5)?,
            avg_latency_ms: row.get(6)?,
            avg_quality: row.get(7)?,
            success_count: row.get(8)?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}
