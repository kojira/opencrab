use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

// ============================================
// SOUL
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoulRow {
    pub agent_id: String,
    pub persona_name: String,
    pub social_style_json: String,
    pub personality_json: String,
    pub thinking_style_json: String,
    pub custom_traits_json: Option<String>,
}

pub fn upsert_soul(conn: &Connection, soul: &SoulRow) -> Result<()> {
    conn.execute(
        "INSERT INTO soul (agent_id, persona_name, social_style_json, personality_json, thinking_style_json, custom_traits_json, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(agent_id) DO UPDATE SET
            persona_name = excluded.persona_name,
            social_style_json = excluded.social_style_json,
            personality_json = excluded.personality_json,
            thinking_style_json = excluded.thinking_style_json,
            custom_traits_json = excluded.custom_traits_json,
            updated_at = excluded.updated_at",
        params![
            soul.agent_id,
            soul.persona_name,
            soul.social_style_json,
            soul.personality_json,
            soul.thinking_style_json,
            soul.custom_traits_json,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_soul(conn: &Connection, agent_id: &str) -> Result<Option<SoulRow>> {
    let result = conn.query_row(
        "SELECT agent_id, persona_name, social_style_json, personality_json, thinking_style_json, custom_traits_json
         FROM soul WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(SoulRow {
                agent_id: row.get(0)?,
                persona_name: row.get(1)?,
                social_style_json: row.get(2)?,
                personality_json: row.get(3)?,
                thinking_style_json: row.get(4)?,
                custom_traits_json: row.get(5)?,
            })
        },
    );

    match result {
        Ok(soul) => Ok(Some(soul)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ============================================
// IDENTITY
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityRow {
    pub agent_id: String,
    pub name: String,
    pub role: String,
    pub job_title: Option<String>,
    pub organization: Option<String>,
    pub image_url: Option<String>,
    pub metadata_json: Option<String>,
}

pub fn upsert_identity(conn: &Connection, identity: &IdentityRow) -> Result<()> {
    conn.execute(
        "INSERT INTO identity (agent_id, name, role, job_title, organization, image_url, metadata_json, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(agent_id) DO UPDATE SET
            name = excluded.name,
            role = excluded.role,
            job_title = excluded.job_title,
            organization = excluded.organization,
            image_url = excluded.image_url,
            metadata_json = excluded.metadata_json,
            updated_at = excluded.updated_at",
        params![
            identity.agent_id,
            identity.name,
            identity.role,
            identity.job_title,
            identity.organization,
            identity.image_url,
            identity.metadata_json,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_identity(conn: &Connection, agent_id: &str) -> Result<Option<IdentityRow>> {
    let result = conn.query_row(
        "SELECT agent_id, name, role, job_title, organization, image_url, metadata_json
         FROM identity WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(IdentityRow {
                agent_id: row.get(0)?,
                name: row.get(1)?,
                role: row.get(2)?,
                job_title: row.get(3)?,
                organization: row.get(4)?,
                image_url: row.get(5)?,
                metadata_json: row.get(6)?,
            })
        },
    );

    match result {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Delete an agent and all related data (identity, soul, skills, curated memory, discord config).
pub fn delete_agent(conn: &Connection, agent_id: &str) -> Result<bool> {
    let deleted = conn.execute("DELETE FROM identity WHERE agent_id = ?1", params![agent_id])?;
    conn.execute("DELETE FROM soul WHERE agent_id = ?1", params![agent_id])?;
    conn.execute("DELETE FROM skills WHERE agent_id = ?1", params![agent_id])?;
    conn.execute(
        "DELETE FROM memory_curated WHERE agent_id = ?1",
        params![agent_id],
    )?;
    conn.execute(
        "DELETE FROM agent_discord_config WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(deleted > 0)
}

/// Find agents by partial ID prefix or name (case-insensitive).
pub fn find_agents(
    conn: &Connection,
    query: &str,
) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, name FROM identity WHERE agent_id LIKE ?1 OR LOWER(name) LIKE LOWER(?2)",
    )?;
    let rows = stmt.query_map(
        params![format!("{}%", query), format!("%{}%", query)],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

// ============================================
// MEMORY: Curated
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratedMemoryRow {
    pub id: String,
    pub agent_id: String,
    pub category: String,
    pub content: String,
}

pub fn upsert_curated_memory(conn: &Connection, memory: &CuratedMemoryRow) -> Result<()> {
    conn.execute(
        "INSERT INTO memory_curated (id, agent_id, category, content, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
            content = excluded.content,
            updated_at = excluded.updated_at",
        params![
            memory.id,
            memory.agent_id,
            memory.category,
            memory.content,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_curated_memories(
    conn: &Connection,
    agent_id: &str,
    category: &str,
) -> Result<Vec<CuratedMemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, category, content FROM memory_curated
         WHERE agent_id = ?1 AND category = ?2 ORDER BY updated_at DESC",
    )?;

    let rows = stmt.query_map(params![agent_id, category], |row| {
        Ok(CuratedMemoryRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            category: row.get(2)?,
            content: row.get(3)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn list_curated_memories(
    conn: &Connection,
    agent_id: &str,
) -> Result<Vec<CuratedMemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, category, content FROM memory_curated
         WHERE agent_id = ?1 ORDER BY updated_at DESC",
    )?;

    let rows = stmt.query_map(params![agent_id], |row| {
        Ok(CuratedMemoryRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            category: row.get(2)?,
            content: row.get(3)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

// ============================================
// MEMORY: Sessions
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLogRow {
    pub id: Option<i64>,
    pub agent_id: String,
    pub session_id: String,
    pub log_type: String,
    pub content: String,
    pub speaker_id: Option<String>,
    pub turn_number: Option<i32>,
    pub metadata_json: Option<String>,
}

pub fn insert_session_log(conn: &Connection, log: &SessionLogRow) -> Result<i64> {
    conn.execute(
        "INSERT INTO memory_sessions (agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            log.agent_id,
            log.session_id,
            log.log_type,
            log.content,
            log.speaker_id,
            log.turn_number,
            log.metadata_json,
            Utc::now().to_rfc3339(),
        ],
    )?;

    let row_id = conn.last_insert_rowid();

    // FTSにも追加
    conn.execute(
        "INSERT INTO memory_sessions_fts (rowid, content, agent_id, session_id, log_type)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![row_id, log.content, log.agent_id, log.session_id, log.log_type],
    )?;

    Ok(row_id)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLogResult {
    pub id: i64,
    pub session_id: String,
    pub log_type: String,
    pub content: String,
    pub created_at: String,
    pub score: f64,
}

pub fn search_session_logs(
    conn: &Connection,
    agent_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SessionLogResult>> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    let fts_query = tokens.join(" AND ");

    let mut stmt = conn.prepare(
        "SELECT ms.id, ms.session_id, ms.log_type, ms.content, ms.created_at, bm25(memory_sessions_fts) as score
         FROM memory_sessions_fts fts
         JOIN memory_sessions ms ON fts.rowid = ms.id
         WHERE fts.agent_id = ?1 AND memory_sessions_fts MATCH ?2
         ORDER BY score
         LIMIT ?3",
    )?;

    let rows = stmt.query_map(params![agent_id, fts_query, limit as i64], |row| {
        Ok(SessionLogResult {
            id: row.get(0)?,
            session_id: row.get(1)?,
            log_type: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
            score: row.get(5)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// List all session logs for a given session, ordered by creation time.
/// Used for building conversation history in send_message.
pub fn list_session_logs_by_session(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json
         FROM memory_sessions WHERE session_id = ?1 ORDER BY id ASC",
    )?;

    let rows = stmt.query_map(params![session_id], |row| {
        Ok(SessionLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            log_type: row.get(3)?,
            content: row.get(4)?,
            speaker_id: row.get(5)?,
            turn_number: row.get(6)?,
            metadata_json: row.get(7)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

// ============================================
// Skills
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRow {
    pub id: String,
    pub agent_id: String,
    pub name: String,
    pub description: String,
    pub situation_pattern: String,
    pub guidance: String,
    pub source_type: String,
    pub source_context: Option<String>,
    pub file_path: Option<String>,
    pub effectiveness: Option<f64>,
    pub usage_count: i32,
    pub is_active: bool,
}

pub fn list_skills(conn: &Connection, agent_id: &str, active_only: bool) -> Result<Vec<SkillRow>> {
    let sql = if active_only {
        "SELECT id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active
         FROM skills WHERE agent_id = ?1 AND is_active = 1 ORDER BY usage_count DESC"
    } else {
        "SELECT id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active
         FROM skills WHERE agent_id = ?1 ORDER BY usage_count DESC"
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![agent_id], |row| {
        Ok(SkillRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            name: row.get(2)?,
            description: row.get(3)?,
            situation_pattern: row.get(4)?,
            guidance: row.get(5)?,
            source_type: row.get(6)?,
            source_context: row.get(7)?,
            file_path: row.get(8)?,
            effectiveness: row.get(9)?,
            usage_count: row.get(10)?,
            is_active: row.get(11)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn insert_skill(conn: &Connection, skill: &SkillRow) -> Result<()> {
    conn.execute(
        "INSERT INTO skills (id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            skill.id,
            skill.agent_id,
            skill.name,
            skill.description,
            skill.situation_pattern,
            skill.guidance,
            skill.source_type,
            skill.source_context,
            skill.file_path,
            skill.effectiveness,
            skill.usage_count,
            skill.is_active,
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn increment_skill_usage(conn: &Connection, skill_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE skills SET usage_count = usage_count + 1, last_used_at = ?1 WHERE id = ?2",
        params![Utc::now().to_rfc3339(), skill_id],
    )?;
    Ok(())
}

pub fn set_skill_active(conn: &Connection, skill_id: &str, active: bool) -> Result<()> {
    conn.execute(
        "UPDATE skills SET is_active = ?1, updated_at = ?2 WHERE id = ?3",
        params![active, Utc::now().to_rfc3339(), skill_id],
    )?;
    Ok(())
}

// ============================================
// Impressions
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpressionRow {
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub target_id: String,
    pub target_name: String,
    pub personality: String,
    pub communication_style: String,
    pub recent_behavior: String,
    pub agreement: String,
    pub notes: String,
    pub last_updated_turn: i32,
}

pub fn upsert_impression(conn: &Connection, imp: &ImpressionRow) -> Result<()> {
    conn.execute(
        "INSERT INTO impressions (id, agent_id, session_id, target_id, target_name, personality, communication_style, recent_behavior, agreement, notes, last_updated_turn, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(agent_id, session_id, target_id) DO UPDATE SET
            personality = excluded.personality,
            communication_style = excluded.communication_style,
            recent_behavior = excluded.recent_behavior,
            agreement = excluded.agreement,
            notes = excluded.notes,
            last_updated_turn = excluded.last_updated_turn,
            updated_at = excluded.updated_at",
        params![
            imp.id,
            imp.agent_id,
            imp.session_id,
            imp.target_id,
            imp.target_name,
            imp.personality,
            imp.communication_style,
            imp.recent_behavior,
            imp.agreement,
            imp.notes,
            imp.last_updated_turn,
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_impressions(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Vec<ImpressionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, target_id, target_name, personality, communication_style, recent_behavior, agreement, notes, last_updated_turn
         FROM impressions WHERE agent_id = ?1 AND session_id = ?2",
    )?;

    let rows = stmt.query_map(params![agent_id, session_id], |row| {
        Ok(ImpressionRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            target_id: row.get(3)?,
            target_name: row.get(4)?,
            personality: row.get(5)?,
            communication_style: row.get(6)?,
            recent_behavior: row.get(7)?,
            agreement: row.get(8)?,
            notes: row.get(9)?,
            last_updated_turn: row.get(10)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

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

pub fn update_llm_metrics_tags(
    conn: &Connection,
    metrics_id: &str,
    tags_json: &str,
) -> Result<()> {
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
    let (sql, param_values): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(model) = model_filter {
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
    let params_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
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
    let (sql, param_values): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(model) = model_filter {
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
    let params_refs: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|p| p.as_ref()).collect();
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

// ============================================
// Sessions
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub mode: String,
    pub theme: String,
    pub phase: String,
    pub turn_number: i32,
    pub status: String,
    pub participant_ids_json: String,
    pub facilitator_id: Option<String>,
    pub done_count: i32,
    pub max_turns: Option<i32>,
}

pub fn insert_session(conn: &Connection, session: &SessionRow) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (id, mode, theme, phase, turn_number, status, participant_ids_json, facilitator_id, done_count, max_turns, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            session.id,
            session.mode,
            session.theme,
            session.phase,
            session.turn_number,
            session.status,
            session.participant_ids_json,
            session.facilitator_id,
            session.done_count,
            session.max_turns,
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_session(conn: &Connection, session_id: &str) -> Result<Option<SessionRow>> {
    let result = conn.query_row(
        "SELECT id, mode, theme, phase, turn_number, status, participant_ids_json, facilitator_id, done_count, max_turns
         FROM sessions WHERE id = ?1",
        params![session_id],
        |row| {
            Ok(SessionRow {
                id: row.get(0)?,
                mode: row.get(1)?,
                theme: row.get(2)?,
                phase: row.get(3)?,
                turn_number: row.get(4)?,
                status: row.get(5)?,
                participant_ids_json: row.get(6)?,
                facilitator_id: row.get(7)?,
                done_count: row.get(8)?,
                max_turns: row.get(9)?,
            })
        },
    );

    match result {
        Ok(session) => Ok(Some(session)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn list_sessions(conn: &Connection) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, mode, theme, phase, turn_number, status, participant_ids_json, facilitator_id, done_count, max_turns
         FROM sessions ORDER BY created_at DESC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(SessionRow {
            id: row.get(0)?,
            mode: row.get(1)?,
            theme: row.get(2)?,
            phase: row.get(3)?,
            turn_number: row.get(4)?,
            status: row.get(5)?,
            participant_ids_json: row.get(6)?,
            facilitator_id: row.get(7)?,
            done_count: row.get(8)?,
            max_turns: row.get(9)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

// ============================================
// Heartbeat Log
// ============================================

pub fn insert_heartbeat_log(
    conn: &Connection,
    agent_id: &str,
    decision: &str,
    result_json: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO heartbeat_log (agent_id, decision, result_json, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![agent_id, decision, result_json, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

// ============================================
// Model Pricing
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricingRow {
    pub provider: String,
    pub model: String,
    pub input_price_per_1m: f64,
    pub output_price_per_1m: f64,
    pub context_window: Option<i32>,
}

pub fn upsert_model_pricing(conn: &Connection, pricing: &ModelPricingRow) -> Result<()> {
    conn.execute(
        "INSERT INTO model_pricing (provider, model, input_price_per_1m, output_price_per_1m, context_window, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(provider, model) DO UPDATE SET
            input_price_per_1m = excluded.input_price_per_1m,
            output_price_per_1m = excluded.output_price_per_1m,
            context_window = excluded.context_window,
            updated_at = excluded.updated_at",
        params![
            pricing.provider,
            pricing.model,
            pricing.input_price_per_1m,
            pricing.output_price_per_1m,
            pricing.context_window,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_model_pricing(
    conn: &Connection,
    provider: &str,
    model: &str,
) -> Result<Option<ModelPricingRow>> {
    let result = conn.query_row(
        "SELECT provider, model, input_price_per_1m, output_price_per_1m, context_window
         FROM model_pricing WHERE provider = ?1 AND model = ?2",
        params![provider, model],
        |row| {
            Ok(ModelPricingRow {
                provider: row.get(0)?,
                model: row.get(1)?,
                input_price_per_1m: row.get(2)?,
                output_price_per_1m: row.get(3)?,
                context_window: row.get(4)?,
            })
        },
    );

    match result {
        Ok(p) => Ok(Some(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ============================================
// Discord Channel Config
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfigRow {
    pub channel_id: String,
    pub guild_id: String,
    pub channel_name: String,
    pub readable: bool,
    pub writable: bool,
}

pub fn get_channel_config(
    conn: &Connection,
    channel_id: &str,
) -> Result<Option<ChannelConfigRow>> {
    let result = conn.query_row(
        "SELECT channel_id, guild_id, channel_name, readable, writable
         FROM discord_channel_config WHERE channel_id = ?1",
        params![channel_id],
        |row| {
            Ok(ChannelConfigRow {
                channel_id: row.get(0)?,
                guild_id: row.get(1)?,
                channel_name: row.get(2)?,
                readable: row.get(3)?,
                writable: row.get(4)?,
            })
        },
    );

    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_channel_config(conn: &Connection, cfg: &ChannelConfigRow) -> Result<()> {
    conn.execute(
        "INSERT INTO discord_channel_config (channel_id, guild_id, channel_name, readable, writable, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(channel_id) DO UPDATE SET
            guild_id = excluded.guild_id,
            channel_name = excluded.channel_name,
            readable = excluded.readable,
            writable = excluded.writable,
            updated_at = excluded.updated_at",
        params![
            cfg.channel_id,
            cfg.guild_id,
            cfg.channel_name,
            cfg.readable,
            cfg.writable,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn list_channel_configs_by_guild(
    conn: &Connection,
    guild_id: &str,
) -> Result<Vec<ChannelConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT channel_id, guild_id, channel_name, readable, writable
         FROM discord_channel_config WHERE guild_id = ?1 ORDER BY channel_name",
    )?;

    let rows = stmt.query_map(params![guild_id], |row| {
        Ok(ChannelConfigRow {
            channel_id: row.get(0)?,
            guild_id: row.get(1)?,
            channel_name: row.get(2)?,
            readable: row.get(3)?,
            writable: row.get(4)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

// ============================================
// Agent Discord Config
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDiscordConfigRow {
    pub agent_id: String,
    pub bot_token: String,
    pub owner_discord_id: String,
    pub enabled: bool,
}

pub fn upsert_agent_discord_config(conn: &Connection, cfg: &AgentDiscordConfigRow) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_discord_config (agent_id, bot_token, owner_discord_id, enabled, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent_id) DO UPDATE SET
            bot_token = excluded.bot_token,
            owner_discord_id = excluded.owner_discord_id,
            enabled = excluded.enabled,
            updated_at = excluded.updated_at",
        params![
            cfg.agent_id,
            cfg.bot_token,
            cfg.owner_discord_id,
            cfg.enabled,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_agent_discord_config(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<AgentDiscordConfigRow>> {
    let result = conn.query_row(
        "SELECT agent_id, bot_token, owner_discord_id, enabled
         FROM agent_discord_config WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(AgentDiscordConfigRow {
                agent_id: row.get(0)?,
                bot_token: row.get(1)?,
                owner_discord_id: row.get(2)?,
                enabled: row.get(3)?,
            })
        },
    );

    match result {
        Ok(cfg) => Ok(Some(cfg)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_agent_discord_config(conn: &Connection, agent_id: &str) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM agent_discord_config WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(deleted > 0)
}

pub fn set_agent_discord_config_enabled(
    conn: &Connection,
    agent_id: &str,
    enabled: bool,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE agent_discord_config SET enabled = ?1, updated_at = ?2 WHERE agent_id = ?3",
        params![enabled, Utc::now().to_rfc3339(), agent_id],
    )?;
    Ok(updated > 0)
}

pub fn list_enabled_agent_discord_configs(conn: &Connection) -> Result<Vec<AgentDiscordConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, bot_token, owner_discord_id, enabled
         FROM agent_discord_config WHERE enabled = 1",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(AgentDiscordConfigRow {
            agent_id: row.get(0)?,
            bot_token: row.get(1)?,
            owner_discord_id: row.get(2)?,
            enabled: row.get(3)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// チャンネルが読み取り可能か判定する。設定なし=true（デフォルト許可）。
pub fn is_channel_readable(conn: &Connection, channel_id: &str) -> bool {
    get_channel_config(conn, channel_id)
        .ok()
        .flatten()
        .map(|c| c.readable)
        .unwrap_or(true)
}

/// チャンネルが書き込み可能か判定する。設定なし=true（デフォルト許可）。
pub fn is_channel_writable(conn: &Connection, channel_id: &str) -> bool {
    get_channel_config(conn, channel_id)
        .ok()
        .flatten()
        .map(|c| c.writable)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        crate::init_memory().expect("failed to init in-memory DB")
    }

    // 1. test_soul_upsert_and_get
    #[test]
    fn test_soul_upsert_and_get() {
        let conn = setup();
        let soul = SoulRow {
            agent_id: "agent-1".to_string(),
            persona_name: "Crab".to_string(),
            social_style_json: r#"{"style":"friendly"}"#.to_string(),
            personality_json: r#"{"trait":"curious"}"#.to_string(),
            thinking_style_json: r#"{"approach":"analytical"}"#.to_string(),
            custom_traits_json: Some(r#"{"hobby":"coding"}"#.to_string()),
        };

        upsert_soul(&conn, &soul).unwrap();

        let fetched = get_soul(&conn, "agent-1").unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.agent_id, "agent-1");
        assert_eq!(fetched.persona_name, "Crab");
        assert_eq!(fetched.social_style_json, r#"{"style":"friendly"}"#);
        assert_eq!(fetched.personality_json, r#"{"trait":"curious"}"#);
        assert_eq!(fetched.thinking_style_json, r#"{"approach":"analytical"}"#);
        assert_eq!(
            fetched.custom_traits_json,
            Some(r#"{"hobby":"coding"}"#.to_string())
        );
    }

    // 2. test_soul_get_nonexistent
    #[test]
    fn test_soul_get_nonexistent() {
        let conn = setup();
        let result = get_soul(&conn, "nonexistent-agent").unwrap();
        assert!(result.is_none());
    }

    // 3. test_identity_upsert_and_get
    #[test]
    fn test_identity_upsert_and_get() {
        let conn = setup();
        let identity = IdentityRow {
            agent_id: "agent-1".to_string(),
            name: "Alice".to_string(),
            role: "facilitator".to_string(),
            job_title: Some("Engineer".to_string()),
            organization: Some("OpenCrab Inc.".to_string()),
            image_url: Some("https://example.com/avatar.png".to_string()),
            metadata_json: Some(r#"{"lang":"en"}"#.to_string()),
        };

        upsert_identity(&conn, &identity).unwrap();

        let fetched = get_identity(&conn, "agent-1").unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.agent_id, "agent-1");
        assert_eq!(fetched.name, "Alice");
        assert_eq!(fetched.role, "facilitator");
        assert_eq!(fetched.job_title, Some("Engineer".to_string()));
        assert_eq!(fetched.organization, Some("OpenCrab Inc.".to_string()));
        assert_eq!(
            fetched.image_url,
            Some("https://example.com/avatar.png".to_string())
        );
        assert_eq!(
            fetched.metadata_json,
            Some(r#"{"lang":"en"}"#.to_string())
        );
    }

    // 4. test_curated_memory_crud
    #[test]
    fn test_curated_memory_crud() {
        let conn = setup();

        let mem1 = CuratedMemoryRow {
            id: "mem-1".to_string(),
            agent_id: "agent-1".to_string(),
            category: "facts".to_string(),
            content: "Rust is a systems programming language.".to_string(),
        };
        let mem2 = CuratedMemoryRow {
            id: "mem-2".to_string(),
            agent_id: "agent-1".to_string(),
            category: "facts".to_string(),
            content: "Crabs have ten legs.".to_string(),
        };

        upsert_curated_memory(&conn, &mem1).unwrap();
        upsert_curated_memory(&conn, &mem2).unwrap();

        let results = get_curated_memories(&conn, "agent-1", "facts").unwrap();
        assert_eq!(results.len(), 2);
    }

    // 5. test_curated_memory_list_all
    #[test]
    fn test_curated_memory_list_all() {
        let conn = setup();

        let mem1 = CuratedMemoryRow {
            id: "mem-1".to_string(),
            agent_id: "agent-1".to_string(),
            category: "facts".to_string(),
            content: "The sky is blue.".to_string(),
        };
        let mem2 = CuratedMemoryRow {
            id: "mem-2".to_string(),
            agent_id: "agent-1".to_string(),
            category: "opinions".to_string(),
            content: "Rust is great.".to_string(),
        };

        upsert_curated_memory(&conn, &mem1).unwrap();
        upsert_curated_memory(&conn, &mem2).unwrap();

        let all = list_curated_memories(&conn, "agent-1").unwrap();
        assert_eq!(all.len(), 2);

        let categories: Vec<&str> = all.iter().map(|m| m.category.as_str()).collect();
        assert!(categories.contains(&"facts"));
        assert!(categories.contains(&"opinions"));
    }

    // 6. test_session_log_insert_and_fts
    #[test]
    fn test_session_log_insert_and_fts() {
        let conn = setup();

        let log1 = SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "The weather is sunny today.".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
        };
        let log2 = SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "I enjoy programming in Rust.".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: Some(2),
            metadata_json: None,
        };
        let log3 = SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Crabs live near the ocean.".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: Some(3),
            metadata_json: None,
        };

        insert_session_log(&conn, &log1).unwrap();
        insert_session_log(&conn, &log2).unwrap();
        insert_session_log(&conn, &log3).unwrap();

        let results = search_session_logs(&conn, "agent-1", "sunny", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("sunny"));
    }

    // 7. test_fts_multi_word_search
    #[test]
    fn test_fts_multi_word_search() {
        let conn = setup();

        let log1 = SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Quantum computing will revolutionize cryptography.".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
        };
        let log2 = SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Classical computing is still dominant.".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: Some(2),
            metadata_json: None,
        };

        insert_session_log(&conn, &log1).unwrap();
        insert_session_log(&conn, &log2).unwrap();

        let results =
            search_session_logs(&conn, "agent-1", "quantum cryptography", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("Quantum"));
    }

    // 8. test_fts_no_results
    #[test]
    fn test_fts_no_results() {
        let conn = setup();

        let log = SessionLogRow {
            id: None,
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            log_type: "message".to_string(),
            content: "Hello world from the test.".to_string(),
            speaker_id: Some("agent-1".to_string()),
            turn_number: Some(1),
            metadata_json: None,
        };
        insert_session_log(&conn, &log).unwrap();

        let results =
            search_session_logs(&conn, "agent-1", "nonexistenttermxyz", 10).unwrap();
        assert!(results.is_empty());
    }

    // 9. test_skills_crud
    #[test]
    fn test_skills_crud() {
        let conn = setup();

        let skill = SkillRow {
            id: "skill-1".to_string(),
            agent_id: "agent-1".to_string(),
            name: "Summarization".to_string(),
            description: "Summarize long texts concisely.".to_string(),
            situation_pattern: "when asked to summarize".to_string(),
            guidance: "Extract key points and present them briefly.".to_string(),
            source_type: "acquired".to_string(),
            source_context: Some("learned from session-1".to_string()),
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
        };

        insert_skill(&conn, &skill).unwrap();

        let skills = list_skills(&conn, "agent-1", true).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "skill-1");
        assert_eq!(skills[0].name, "Summarization");
        assert!(skills[0].is_active);
        assert_eq!(skills[0].usage_count, 0);
        assert_eq!(skills[0].source_type, "acquired");
    }

    // 10. test_skill_usage_increment
    #[test]
    fn test_skill_usage_increment() {
        let conn = setup();

        let skill = SkillRow {
            id: "skill-1".to_string(),
            agent_id: "agent-1".to_string(),
            name: "Translation".to_string(),
            description: "Translate between languages.".to_string(),
            situation_pattern: "when translation is needed".to_string(),
            guidance: "Use context-aware translation.".to_string(),
            source_type: "acquired".to_string(),
            source_context: None,
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
        };

        insert_skill(&conn, &skill).unwrap();
        increment_skill_usage(&conn, "skill-1").unwrap();

        let skills = list_skills(&conn, "agent-1", true).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].usage_count, 1);
    }

    // 11. test_impressions_upsert_and_get
    #[test]
    fn test_impressions_upsert_and_get() {
        let conn = setup();

        let impression = ImpressionRow {
            id: "imp-1".to_string(),
            agent_id: "agent-1".to_string(),
            session_id: "session-1".to_string(),
            target_id: "agent-2".to_string(),
            target_name: "Bob".to_string(),
            personality: "thoughtful and calm".to_string(),
            communication_style: "concise".to_string(),
            recent_behavior: "asked good questions".to_string(),
            agreement: "mostly agree".to_string(),
            notes: "potential collaborator".to_string(),
            last_updated_turn: 5,
        };

        upsert_impression(&conn, &impression).unwrap();

        let results = get_impressions(&conn, "agent-1", "session-1").unwrap();
        assert_eq!(results.len(), 1);
        let fetched = &results[0];
        assert_eq!(fetched.id, "imp-1");
        assert_eq!(fetched.target_id, "agent-2");
        assert_eq!(fetched.target_name, "Bob");
        assert_eq!(fetched.personality, "thoughtful and calm");
        assert_eq!(fetched.communication_style, "concise");
        assert_eq!(fetched.recent_behavior, "asked good questions");
        assert_eq!(fetched.agreement, "mostly agree");
        assert_eq!(fetched.notes, "potential collaborator");
        assert_eq!(fetched.last_updated_turn, 5);
    }

    // 12. test_session_crud
    #[test]
    fn test_session_crud() {
        let conn = setup();

        let session = SessionRow {
            id: "session-1".to_string(),
            mode: "facilitated".to_string(),
            theme: "AI Ethics Discussion".to_string(),
            phase: "divergent".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: r#"["agent-1","agent-2"]"#.to_string(),
            facilitator_id: Some("agent-1".to_string()),
            done_count: 0,
            max_turns: Some(10),
        };

        insert_session(&conn, &session).unwrap();

        let fetched = get_session(&conn, "session-1").unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, "session-1");
        assert_eq!(fetched.mode, "facilitated");
        assert_eq!(fetched.theme, "AI Ethics Discussion");
        assert_eq!(fetched.phase, "divergent");
        assert_eq!(fetched.turn_number, 0);
        assert_eq!(fetched.status, "active");
        assert_eq!(fetched.facilitator_id, Some("agent-1".to_string()));
        assert_eq!(fetched.max_turns, Some(10));

        let all = list_sessions(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "session-1");
    }

    // 13. test_llm_metrics_insert_and_summary
    #[test]
    fn test_llm_metrics_insert_and_summary() {
        let conn = setup();

        let metrics1 = LlmMetricsRow {
            id: "metrics-1".to_string(),
            agent_id: "agent-1".to_string(),
            session_id: Some("session-1".to_string()),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            purpose: "discussion".to_string(),
            task_type: Some("chat".to_string()),
            complexity: Some("medium".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            estimated_cost_usd: 0.005,
            latency_ms: 1200,
            time_to_first_token_ms: Some(200),
        };

        let metrics2 = LlmMetricsRow {
            id: "metrics-2".to_string(),
            agent_id: "agent-1".to_string(),
            session_id: Some("session-1".to_string()),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            purpose: "summarization".to_string(),
            task_type: Some("summary".to_string()),
            complexity: Some("low".to_string()),
            input_tokens: 200,
            output_tokens: 80,
            total_tokens: 280,
            estimated_cost_usd: 0.008,
            latency_ms: 800,
            time_to_first_token_ms: Some(150),
        };

        insert_llm_metrics(&conn, &metrics1).unwrap();
        insert_llm_metrics(&conn, &metrics2).unwrap();

        let summary =
            get_llm_metrics_summary(&conn, "agent-1", "2020-01-01").unwrap();
        assert_eq!(summary.count, 2);
        assert_eq!(summary.total_tokens, Some(430));
        let total_cost = summary.total_cost.unwrap();
        assert!((total_cost - 0.013).abs() < 1e-9);
        let avg_latency = summary.avg_latency.unwrap();
        assert!((avg_latency - 1000.0).abs() < 1e-9);
    }

    // 14. test_llm_metrics_evaluation_update
    #[test]
    fn test_llm_metrics_evaluation_update() {
        let conn = setup();

        let metrics = LlmMetricsRow {
            id: "metrics-1".to_string(),
            agent_id: "agent-1".to_string(),
            session_id: Some("session-1".to_string()),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            purpose: "discussion".to_string(),
            task_type: Some("chat".to_string()),
            complexity: Some("medium".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            estimated_cost_usd: 0.005,
            latency_ms: 1200,
            time_to_first_token_ms: Some(200),
        };

        insert_llm_metrics(&conn, &metrics).unwrap();
        update_llm_metrics_evaluation(&conn, "metrics-1", 0.95, true, "excellent response")
            .unwrap();

        // Read back via raw SQL to verify the evaluation columns
        let (quality_score, task_success, self_evaluation): (f64, i32, String) = conn
            .query_row(
                "SELECT quality_score, task_success, self_evaluation FROM llm_usage_metrics WHERE id = ?1",
                params!["metrics-1"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert!((quality_score - 0.95).abs() < 1e-9);
        assert_eq!(task_success, 1);
        assert_eq!(self_evaluation, "excellent response");
    }

    // 14b. test_llm_metrics_by_model
    #[test]
    fn test_llm_metrics_by_model() {
        let conn = setup();

        let m1 = LlmMetricsRow {
            id: "m-1".to_string(),
            agent_id: "agent-1".to_string(),
            session_id: Some("s-1".to_string()),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            purpose: "conversation".to_string(),
            task_type: Some("chat".to_string()),
            complexity: Some("medium".to_string()),
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            estimated_cost_usd: 0.005,
            latency_ms: 1200,
            time_to_first_token_ms: Some(200),
        };
        let m2 = LlmMetricsRow {
            id: "m-2".to_string(),
            agent_id: "agent-1".to_string(),
            session_id: Some("s-1".to_string()),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            purpose: "conversation".to_string(),
            task_type: Some("chat".to_string()),
            complexity: Some("low".to_string()),
            input_tokens: 80,
            output_tokens: 40,
            total_tokens: 120,
            estimated_cost_usd: 0.001,
            latency_ms: 400,
            time_to_first_token_ms: Some(100),
        };
        let m3 = LlmMetricsRow {
            id: "m-3".to_string(),
            agent_id: "agent-1".to_string(),
            session_id: Some("s-1".to_string()),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            purpose: "analysis".to_string(),
            task_type: Some("summary".to_string()),
            complexity: Some("low".to_string()),
            input_tokens: 60,
            output_tokens: 30,
            total_tokens: 90,
            estimated_cost_usd: 0.0008,
            latency_ms: 300,
            time_to_first_token_ms: Some(80),
        };

        insert_llm_metrics(&conn, &m1).unwrap();
        insert_llm_metrics(&conn, &m2).unwrap();
        insert_llm_metrics(&conn, &m3).unwrap();

        let stats = get_llm_metrics_by_model(&conn, "agent-1", "2020-01-01").unwrap();
        assert_eq!(stats.len(), 2);

        // gpt-4o-mini has 2 records, gpt-4o has 1 → sorted by count DESC
        assert_eq!(stats[0].model, "gpt-4o-mini");
        assert_eq!(stats[0].count, 2);
        assert_eq!(stats[0].total_tokens, 210);
        assert!((stats[0].total_cost - 0.0018).abs() < 1e-9);

        assert_eq!(stats[1].model, "gpt-4o");
        assert_eq!(stats[1].count, 1);
    }

    // 14c. test_llm_metrics_by_model_and_purpose
    #[test]
    fn test_llm_metrics_by_model_and_purpose() {
        let conn = setup();

        // gpt-4o for conversation
        let m1 = LlmMetricsRow {
            id: "mp-1".to_string(),
            agent_id: "agent-1".to_string(),
            session_id: Some("s-1".to_string()),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4o".to_string(),
            purpose: "conversation".to_string(),
            task_type: Some("chat".to_string()),
            complexity: None,
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            estimated_cost_usd: 0.005,
            latency_ms: 2000,
            time_to_first_token_ms: None,
        };
        // gpt-4o for analysis
        let m2 = LlmMetricsRow {
            id: "mp-2".to_string(),
            purpose: "analysis".to_string(),
            estimated_cost_usd: 0.008,
            latency_ms: 3000,
            ..m1.clone()
        };
        // gpt-4o-mini for conversation
        let m3 = LlmMetricsRow {
            id: "mp-3".to_string(),
            model: "gpt-4o-mini".to_string(),
            purpose: "conversation".to_string(),
            estimated_cost_usd: 0.001,
            latency_ms: 400,
            ..m1.clone()
        };
        // gpt-4o-mini for analysis
        let m4 = LlmMetricsRow {
            id: "mp-4".to_string(),
            model: "gpt-4o-mini".to_string(),
            purpose: "analysis".to_string(),
            estimated_cost_usd: 0.0015,
            latency_ms: 500,
            ..m1.clone()
        };

        insert_llm_metrics(&conn, &m1).unwrap();
        insert_llm_metrics(&conn, &m2).unwrap();
        insert_llm_metrics(&conn, &m3).unwrap();
        insert_llm_metrics(&conn, &m4).unwrap();

        let stats = get_llm_metrics_by_model_and_purpose(&conn, "agent-1", "2020-01-01").unwrap();
        // Should have 4 entries: (gpt-4o, analysis), (gpt-4o, conversation), (gpt-4o-mini, analysis), (gpt-4o-mini, conversation)
        assert_eq!(stats.len(), 4);

        // Verify each entry has correct purpose.
        let purposes: Vec<&str> = stats.iter().map(|s| s.purpose.as_str()).collect();
        assert!(purposes.contains(&"conversation"));
        assert!(purposes.contains(&"analysis"));

        // Verify we can distinguish same model in different purposes.
        let gpt4o_conv = stats.iter().find(|s| s.model == "gpt-4o" && s.purpose == "conversation").unwrap();
        let gpt4o_anl = stats.iter().find(|s| s.model == "gpt-4o" && s.purpose == "analysis").unwrap();
        assert!((gpt4o_conv.total_cost - 0.005).abs() < 1e-9);
        assert!((gpt4o_anl.total_cost - 0.008).abs() < 1e-9);
    }

    // 15. test_model_pricing_upsert_and_get
    #[test]
    fn test_model_pricing_upsert_and_get() {
        let conn = setup();

        let pricing = ModelPricingRow {
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            input_price_per_1m: 30.0,
            output_price_per_1m: 60.0,
            context_window: Some(128000),
        };

        upsert_model_pricing(&conn, &pricing).unwrap();

        let fetched = get_model_pricing(&conn, "openai", "gpt-4").unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.provider, "openai");
        assert_eq!(fetched.model, "gpt-4");
        assert!((fetched.input_price_per_1m - 30.0).abs() < 1e-9);
        assert!((fetched.output_price_per_1m - 60.0).abs() < 1e-9);
        assert_eq!(fetched.context_window, Some(128000));
    }

    // 16. test_heartbeat_log_insert
    #[test]
    fn test_heartbeat_log_insert() {
        let conn = setup();

        let result = insert_heartbeat_log(
            &conn,
            "agent-1",
            "idle",
            Some(r#"{"action":"none"}"#),
        );
        assert!(result.is_ok());
    }

    // ── delete_agent ──

    #[test]
    fn test_delete_agent() {
        let conn = setup();

        // Create agent with identity, soul, skill, and curated memory
        upsert_identity(
            &conn,
            &IdentityRow {
                agent_id: "del-1".into(),
                name: "DeleteMe".into(),
                role: "test".into(),
                job_title: None,
                organization: None,
                image_url: None,
                metadata_json: None,
            },
        )
        .unwrap();
        upsert_soul(
            &conn,
            &SoulRow {
                agent_id: "del-1".into(),
                persona_name: "Doomed".into(),
                social_style_json: "{}".into(),
                personality_json: "{}".into(),
                thinking_style_json: "{}".into(),
                custom_traits_json: None,
            },
        )
        .unwrap();
        upsert_curated_memory(
            &conn,
            &CuratedMemoryRow {
                id: "cm-del-1".into(),
                agent_id: "del-1".into(),
                category: "fact".into(),
                content: "will be deleted".into(),
            },
        )
        .unwrap();

        // Verify data exists
        assert!(get_identity(&conn, "del-1").unwrap().is_some());
        assert!(get_soul(&conn, "del-1").unwrap().is_some());

        // Delete
        let deleted = delete_agent(&conn, "del-1").unwrap();
        assert!(deleted);

        // Verify everything is gone
        assert!(get_identity(&conn, "del-1").unwrap().is_none());
        assert!(get_soul(&conn, "del-1").unwrap().is_none());
        assert!(list_curated_memories(&conn, "del-1").unwrap().is_empty());
    }

    #[test]
    fn test_delete_agent_nonexistent() {
        let conn = setup();
        let deleted = delete_agent(&conn, "no-such-agent").unwrap();
        assert!(!deleted);
    }

    // ── find_agents ──

    #[test]
    fn test_find_agents_by_id_prefix() {
        let conn = setup();
        upsert_identity(
            &conn,
            &IdentityRow {
                agent_id: "abc-12345".into(),
                name: "Alice".into(),
                role: "test".into(),
                job_title: None,
                organization: None,
                image_url: None,
                metadata_json: None,
            },
        )
        .unwrap();
        upsert_identity(
            &conn,
            &IdentityRow {
                agent_id: "xyz-99999".into(),
                name: "Bob".into(),
                role: "test".into(),
                job_title: None,
                organization: None,
                image_url: None,
                metadata_json: None,
            },
        )
        .unwrap();

        // Search by ID prefix
        let results = find_agents(&conn, "abc").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "Alice");

        // Search by name
        let results = find_agents(&conn, "bob").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "Bob");

        // No match
        let results = find_agents(&conn, "zzz").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_find_agents_partial_name() {
        let conn = setup();
        upsert_identity(
            &conn,
            &IdentityRow {
                agent_id: "agent-find-1".into(),
                name: "Creative Researcher".into(),
                role: "discussant".into(),
                job_title: None,
                organization: None,
                image_url: None,
                metadata_json: None,
            },
        )
        .unwrap();

        let results = find_agents(&conn, "creative").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "Creative Researcher");

        let results = find_agents(&conn, "researcher").unwrap();
        assert_eq!(results.len(), 1);
    }

    // ── Agent CRUD full cycle ──

    #[test]
    fn test_agent_crud_full_cycle() {
        let conn = setup();

        // Create
        let agent_id = "crud-agent-1";
        upsert_identity(
            &conn,
            &IdentityRow {
                agent_id: agent_id.into(),
                name: "TestAgent".into(),
                role: "discussant".into(),
                job_title: None,
                organization: None,
                image_url: None,
                metadata_json: None,
            },
        )
        .unwrap();
        upsert_soul(
            &conn,
            &SoulRow {
                agent_id: agent_id.into(),
                persona_name: "Original Persona".into(),
                social_style_json: "{}".into(),
                personality_json: "{}".into(),
                thinking_style_json: "{}".into(),
                custom_traits_json: None,
            },
        )
        .unwrap();

        // Read
        let identity = get_identity(&conn, agent_id).unwrap().unwrap();
        assert_eq!(identity.name, "TestAgent");
        assert_eq!(identity.role, "discussant");
        let soul = get_soul(&conn, agent_id).unwrap().unwrap();
        assert_eq!(soul.persona_name, "Original Persona");

        // Update
        upsert_identity(
            &conn,
            &IdentityRow {
                agent_id: agent_id.into(),
                name: "UpdatedAgent".into(),
                role: "facilitator".into(),
                job_title: Some("Lead".into()),
                organization: None,
                image_url: None,
                metadata_json: None,
            },
        )
        .unwrap();
        upsert_soul(
            &conn,
            &SoulRow {
                agent_id: agent_id.into(),
                persona_name: "Updated Persona".into(),
                social_style_json: r#"{"style":"analytical"}"#.into(),
                personality_json: "{}".into(),
                thinking_style_json: "{}".into(),
                custom_traits_json: None,
            },
        )
        .unwrap();

        let identity = get_identity(&conn, agent_id).unwrap().unwrap();
        assert_eq!(identity.name, "UpdatedAgent");
        assert_eq!(identity.role, "facilitator");
        assert_eq!(identity.job_title, Some("Lead".to_string()));
        let soul = get_soul(&conn, agent_id).unwrap().unwrap();
        assert_eq!(soul.persona_name, "Updated Persona");

        // Find
        let results = find_agents(&conn, "Updated").unwrap();
        assert_eq!(results.len(), 1);

        // Delete
        let deleted = delete_agent(&conn, agent_id).unwrap();
        assert!(deleted);
        assert!(get_identity(&conn, agent_id).unwrap().is_none());
        assert!(get_soul(&conn, agent_id).unwrap().is_none());

        // Find after delete
        let results = find_agents(&conn, "Updated").unwrap();
        assert!(results.is_empty());
    }

    // ── Discord Channel Config ──

    #[test]
    fn test_channel_config_upsert_and_get() {
        let conn = setup();

        let cfg = ChannelConfigRow {
            channel_id: "123456".to_string(),
            guild_id: "guild-1".to_string(),
            channel_name: "general".to_string(),
            readable: true,
            writable: false,
        };

        upsert_channel_config(&conn, &cfg).unwrap();

        let fetched = get_channel_config(&conn, "123456").unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.channel_id, "123456");
        assert_eq!(fetched.guild_id, "guild-1");
        assert_eq!(fetched.channel_name, "general");
        assert!(fetched.readable);
        assert!(!fetched.writable);
    }

    #[test]
    fn test_channel_config_upsert_update() {
        let conn = setup();

        let cfg = ChannelConfigRow {
            channel_id: "123456".to_string(),
            guild_id: "guild-1".to_string(),
            channel_name: "general".to_string(),
            readable: true,
            writable: true,
        };
        upsert_channel_config(&conn, &cfg).unwrap();

        // Update writable to false
        let cfg2 = ChannelConfigRow {
            writable: false,
            ..cfg
        };
        upsert_channel_config(&conn, &cfg2).unwrap();

        let fetched = get_channel_config(&conn, "123456").unwrap().unwrap();
        assert!(fetched.readable);
        assert!(!fetched.writable);
    }

    #[test]
    fn test_channel_config_list_by_guild() {
        let conn = setup();

        let cfg1 = ChannelConfigRow {
            channel_id: "ch-1".to_string(),
            guild_id: "guild-1".to_string(),
            channel_name: "general".to_string(),
            readable: true,
            writable: true,
        };
        let cfg2 = ChannelConfigRow {
            channel_id: "ch-2".to_string(),
            guild_id: "guild-1".to_string(),
            channel_name: "random".to_string(),
            readable: false,
            writable: true,
        };
        let cfg3 = ChannelConfigRow {
            channel_id: "ch-3".to_string(),
            guild_id: "guild-2".to_string(),
            channel_name: "other".to_string(),
            readable: true,
            writable: true,
        };

        upsert_channel_config(&conn, &cfg1).unwrap();
        upsert_channel_config(&conn, &cfg2).unwrap();
        upsert_channel_config(&conn, &cfg3).unwrap();

        let results = list_channel_configs_by_guild(&conn, "guild-1").unwrap();
        assert_eq!(results.len(), 2);

        let results2 = list_channel_configs_by_guild(&conn, "guild-2").unwrap();
        assert_eq!(results2.len(), 1);
    }

    #[test]
    fn test_is_channel_readable_writable_defaults() {
        let conn = setup();

        // No config → defaults to true
        assert!(is_channel_readable(&conn, "unknown-ch"));
        assert!(is_channel_writable(&conn, "unknown-ch"));

        // Set readable=false
        let cfg = ChannelConfigRow {
            channel_id: "ch-blocked".to_string(),
            guild_id: "guild-1".to_string(),
            channel_name: "blocked".to_string(),
            readable: false,
            writable: false,
        };
        upsert_channel_config(&conn, &cfg).unwrap();

        assert!(!is_channel_readable(&conn, "ch-blocked"));
        assert!(!is_channel_writable(&conn, "ch-blocked"));
    }

    // ── Agent Discord Config ──

    #[test]
    fn test_agent_discord_config_upsert_and_get() {
        let conn = setup();

        let cfg = AgentDiscordConfigRow {
            agent_id: "agent-1".to_string(),
            bot_token: "TOKEN_ABC_12345".to_string(),
            owner_discord_id: "390123456789".to_string(),
            enabled: true,
        };

        upsert_agent_discord_config(&conn, &cfg).unwrap();

        let fetched = get_agent_discord_config(&conn, "agent-1").unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.agent_id, "agent-1");
        assert_eq!(fetched.bot_token, "TOKEN_ABC_12345");
        assert_eq!(fetched.owner_discord_id, "390123456789");
        assert!(fetched.enabled);
    }

    #[test]
    fn test_agent_discord_config_get_nonexistent() {
        let conn = setup();
        let result = get_agent_discord_config(&conn, "no-such-agent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_agent_discord_config_upsert_update() {
        let conn = setup();

        let cfg = AgentDiscordConfigRow {
            agent_id: "agent-1".to_string(),
            bot_token: "OLD_TOKEN".to_string(),
            owner_discord_id: "".to_string(),
            enabled: true,
        };
        upsert_agent_discord_config(&conn, &cfg).unwrap();

        // Update token and owner
        let cfg2 = AgentDiscordConfigRow {
            agent_id: "agent-1".to_string(),
            bot_token: "NEW_TOKEN".to_string(),
            owner_discord_id: "999888777".to_string(),
            enabled: false,
        };
        upsert_agent_discord_config(&conn, &cfg2).unwrap();

        let fetched = get_agent_discord_config(&conn, "agent-1").unwrap().unwrap();
        assert_eq!(fetched.bot_token, "NEW_TOKEN");
        assert_eq!(fetched.owner_discord_id, "999888777");
        assert!(!fetched.enabled);
    }

    #[test]
    fn test_agent_discord_config_delete() {
        let conn = setup();

        let cfg = AgentDiscordConfigRow {
            agent_id: "agent-del".to_string(),
            bot_token: "TOKEN".to_string(),
            owner_discord_id: "".to_string(),
            enabled: true,
        };
        upsert_agent_discord_config(&conn, &cfg).unwrap();
        assert!(get_agent_discord_config(&conn, "agent-del").unwrap().is_some());

        let deleted = delete_agent_discord_config(&conn, "agent-del").unwrap();
        assert!(deleted);
        assert!(get_agent_discord_config(&conn, "agent-del").unwrap().is_none());

        // Delete nonexistent → false
        let deleted2 = delete_agent_discord_config(&conn, "agent-del").unwrap();
        assert!(!deleted2);
    }

    #[test]
    fn test_list_enabled_agent_discord_configs() {
        let conn = setup();

        let cfg1 = AgentDiscordConfigRow {
            agent_id: "a1".to_string(),
            bot_token: "T1".to_string(),
            owner_discord_id: "".to_string(),
            enabled: true,
        };
        let cfg2 = AgentDiscordConfigRow {
            agent_id: "a2".to_string(),
            bot_token: "T2".to_string(),
            owner_discord_id: "".to_string(),
            enabled: false, // disabled
        };
        let cfg3 = AgentDiscordConfigRow {
            agent_id: "a3".to_string(),
            bot_token: "T3".to_string(),
            owner_discord_id: "owner".to_string(),
            enabled: true,
        };

        upsert_agent_discord_config(&conn, &cfg1).unwrap();
        upsert_agent_discord_config(&conn, &cfg2).unwrap();
        upsert_agent_discord_config(&conn, &cfg3).unwrap();

        let enabled = list_enabled_agent_discord_configs(&conn).unwrap();
        assert_eq!(enabled.len(), 2);

        let ids: Vec<&str> = enabled.iter().map(|c| c.agent_id.as_str()).collect();
        assert!(ids.contains(&"a1"));
        assert!(ids.contains(&"a3"));
        assert!(!ids.contains(&"a2"));
    }

    #[test]
    fn test_set_agent_discord_config_enabled() {
        let conn = setup();

        let cfg = AgentDiscordConfigRow {
            agent_id: "agent-toggle".to_string(),
            bot_token: "TOKEN".to_string(),
            owner_discord_id: "".to_string(),
            enabled: true,
        };
        upsert_agent_discord_config(&conn, &cfg).unwrap();

        // Initially enabled
        let fetched = get_agent_discord_config(&conn, "agent-toggle").unwrap().unwrap();
        assert!(fetched.enabled);

        // Disable
        let updated = set_agent_discord_config_enabled(&conn, "agent-toggle", false).unwrap();
        assert!(updated);
        let fetched = get_agent_discord_config(&conn, "agent-toggle").unwrap().unwrap();
        assert!(!fetched.enabled);

        // Re-enable
        let updated = set_agent_discord_config_enabled(&conn, "agent-toggle", true).unwrap();
        assert!(updated);
        let fetched = get_agent_discord_config(&conn, "agent-toggle").unwrap().unwrap();
        assert!(fetched.enabled);

        // Nonexistent agent → false
        let updated = set_agent_discord_config_enabled(&conn, "no-such", false).unwrap();
        assert!(!updated);
    }

    #[test]
    fn test_delete_agent_also_removes_discord_config() {
        let conn = setup();

        let agent_id = "agent-discord-del";
        upsert_identity(
            &conn,
            &IdentityRow {
                agent_id: agent_id.into(),
                name: "DiscordAgent".into(),
                role: "test".into(),
                job_title: None,
                organization: None,
                image_url: None,
                metadata_json: None,
            },
        )
        .unwrap();
        upsert_agent_discord_config(
            &conn,
            &AgentDiscordConfigRow {
                agent_id: agent_id.into(),
                bot_token: "BOT_TOKEN_123".into(),
                owner_discord_id: "owner-1".into(),
                enabled: true,
            },
        )
        .unwrap();

        // Verify exists
        assert!(get_agent_discord_config(&conn, agent_id).unwrap().is_some());

        // Delete agent
        let deleted = delete_agent(&conn, agent_id).unwrap();
        assert!(deleted);

        // Discord config should also be gone
        assert!(get_agent_discord_config(&conn, agent_id).unwrap().is_none());
    }
}
