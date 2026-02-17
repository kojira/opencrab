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
