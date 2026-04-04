use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

// ============================================
// AGENTS (soul + identity 統合)
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRow {
    pub agent_id: String,
    pub name: String,
    pub job_title: Option<String>,
    pub organization: Option<String>,
    pub image_url: Option<String>,
    pub persona_name: String,
    pub personality: Option<String>,
    #[serde(default)]
    pub instructions: String,
    pub model: Option<String>,
    pub metadata_json: Option<String>,
}

/// PATCH 用: 未指定のフィールドは変更しない。`Option<Option<T>>` は JSON の null でクリア。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct AgentPatch {
    pub name: Option<String>,
    pub job_title: Option<Option<String>>,
    pub organization: Option<Option<String>>,
    pub image_url: Option<Option<String>>,
    pub persona_name: Option<String>,
    pub personality: Option<Option<String>>,
    pub instructions: Option<String>,
    pub model: Option<Option<String>>,
    pub metadata_json: Option<Option<String>>,
}

pub fn upsert_agent(conn: &Connection, agent: &AgentRow) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO agents (agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, model, metadata_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(agent_id) DO UPDATE SET
            name = excluded.name,
            job_title = excluded.job_title,
            organization = excluded.organization,
            image_url = excluded.image_url,
            persona_name = excluded.persona_name,
            personality = excluded.personality,
            instructions = excluded.instructions,
            model = excluded.model,
            metadata_json = excluded.metadata_json,
            updated_at = excluded.updated_at",
        params![
            agent.agent_id,
            agent.name,
            agent.job_title,
            agent.organization,
            agent.image_url,
            agent.persona_name,
            agent.personality,
            agent.instructions,
            agent.model,
            agent.metadata_json,
            now,
            now,
        ],
    )?;
    Ok(())
}

pub fn get_agent(conn: &Connection, agent_id: &str) -> Result<Option<AgentRow>> {
    let result = conn.query_row(
        "SELECT agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, model, metadata_json
         FROM agents WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(AgentRow {
                agent_id: row.get(0)?,
                name: row.get(1)?,
                job_title: row.get(2)?,
                organization: row.get(3)?,
                image_url: row.get(4)?,
                persona_name: row.get(5)?,
                personality: row.get(6)?,
                instructions: row.get(7)?,
                model: row.get(8)?,
                metadata_json: row.get(9)?,
            })
        },
    );
    match result {
        Ok(a) => Ok(Some(a)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `agents.model` が空でなければそれを使い、否则は `default_model`（通常は `provider:model`）。
pub fn effective_model_for_agent(
    conn: &Connection,
    agent_id: &str,
    default_model: &str,
) -> Result<String> {
    Ok(get_agent(conn, agent_id)?
        .and_then(|a| a.model)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| default_model.to_string()))
}

pub fn apply_agent_patch(conn: &Connection, agent_id: &str, patch: &AgentPatch) -> Result<bool> {
    let Some(mut row) = get_agent(conn, agent_id)? else {
        return Ok(false);
    };
    if let Some(ref v) = patch.name {
        row.name = v.clone();
    }
    if let Some(ref v) = patch.job_title {
        row.job_title = v.clone();
    }
    if let Some(ref v) = patch.organization {
        row.organization = v.clone();
    }
    if let Some(ref v) = patch.image_url {
        row.image_url = v.clone();
    }
    if let Some(ref v) = patch.persona_name {
        row.persona_name = v.clone();
    }
    if let Some(ref v) = patch.personality {
        row.personality = v.clone();
    }
    if let Some(ref v) = patch.instructions {
        row.instructions = v.clone();
    }
    if let Some(ref v) = patch.model {
        row.model = v.clone();
    }
    if let Some(ref v) = patch.metadata_json {
        row.metadata_json = v.clone();
    }
    upsert_agent(conn, &row)?;
    Ok(true)
}

// ============================================
// SOUL PRESETS
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoulPresetRow {
    pub id: String,
    pub agent_id: String,
    pub preset_name: String,
    pub persona_name: String,
    pub custom_traits_json: Option<String>,
}

pub fn list_soul_presets(conn: &Connection, agent_id: &str) -> Result<Vec<SoulPresetRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, preset_name, persona_name, custom_traits_json
         FROM soul_presets WHERE agent_id = ?1 ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| {
        Ok(SoulPresetRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            preset_name: row.get(2)?,
            persona_name: row.get(3)?,
            custom_traits_json: row.get(4)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn get_soul_preset(conn: &Connection, preset_id: &str) -> Result<Option<SoulPresetRow>> {
    let result = conn.query_row(
        "SELECT id, agent_id, preset_name, persona_name, custom_traits_json
         FROM soul_presets WHERE id = ?1",
        params![preset_id],
        |row| {
            Ok(SoulPresetRow {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                preset_name: row.get(2)?,
                persona_name: row.get(3)?,
                custom_traits_json: row.get(4)?,
            })
        },
    );
    match result {
        Ok(preset) => Ok(Some(preset)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn insert_soul_preset(conn: &Connection, preset: &SoulPresetRow) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO soul_presets (id, agent_id, preset_name, persona_name, custom_traits_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            preset.id,
            preset.agent_id,
            preset.preset_name,
            preset.persona_name,
            preset.custom_traits_json,
            now,
            now,
        ],
    )?;
    Ok(())
}

pub fn delete_soul_preset(conn: &Connection, preset_id: &str) -> Result<bool> {
    let deleted = conn.execute("DELETE FROM soul_presets WHERE id = ?1", params![preset_id])?;
    Ok(deleted > 0)
}

/// Delete an agent and all related data (agents row, skills, curated memory, discord config, presets).
pub fn delete_agent(conn: &Connection, agent_id: &str) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM agents WHERE agent_id = ?1",
        params![agent_id],
    )?;
    conn.execute(
        "DELETE FROM soul_presets WHERE agent_id = ?1",
        params![agent_id],
    )?;
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
pub fn find_agents(conn: &Connection, query: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT agent_id, name FROM agents WHERE agent_id LIKE ?1 OR LOWER(name) LIKE LOWER(?2)",
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
    pub created_at: String,
}

pub fn upsert_curated_memory(conn: &Connection, memory: &CuratedMemoryRow) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memory_curated (id, agent_id, category, content, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent_id, category) DO UPDATE SET
            content = excluded.content,
            updated_at = excluded.updated_at",
        params![
            memory.id,
            memory.agent_id,
            memory.category,
            memory.content,
            now,
            now,
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
        "SELECT id, agent_id, category, content, created_at FROM memory_curated
         WHERE agent_id = ?1 AND category = ?2 ORDER BY updated_at DESC",
    )?;

    let rows = stmt.query_map(params![agent_id, category], |row| {
        Ok(CuratedMemoryRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            category: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn list_curated_memories(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<CuratedMemoryRow>, i64)> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_curated WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, agent_id, category, content, created_at FROM memory_curated
         WHERE agent_id = ?1 ORDER BY created_at ASC LIMIT ?2 OFFSET ?3",
    )?;

    let rows = stmt.query_map(params![agent_id, limit, offset], |row| {
        Ok(CuratedMemoryRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            category: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;

    Ok((rows.collect::<std::result::Result<_, _>>()?, total))
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
    pub created_at: Option<String>,
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
        params![
            row_id,
            log.content,
            log.agent_id,
            log.session_id,
            log.log_type
        ],
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
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
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
            created_at: row.get(8)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Count the number of logs in a session.
pub fn count_session_logs(conn: &Connection, session_id: &str) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// List session logs with id > after_id, ordered by id ASC.
pub fn list_session_logs_after_id(
    conn: &Connection,
    session_id: &str,
    after_id: i64,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 AND id > ?2 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![session_id, after_id], |row| {
        Ok(SessionLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            log_type: row.get(3)?,
            content: row.get(4)?,
            speaker_id: row.get(5)?,
            turn_number: row.get(6)?,
            metadata_json: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// List the most recent N session logs (returned in id DESC order; caller should reverse).
pub fn list_recent_session_logs(
    conn: &Connection,
    session_id: &str,
    limit: usize,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![session_id, limit as i64], |row| {
        Ok(SessionLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            log_type: row.get(3)?,
            content: row.get(4)?,
            speaker_id: row.get(5)?,
            turn_number: row.get(6)?,
            metadata_json: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Get topic nodes for a specific session, ordered by start_log_id ASC.
pub fn get_topic_nodes_for_session(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id, end_log_id, source_session_id, date_from, date_to, depth, child_count, token_count, created_at, updated_at, short_id
         FROM memory_index_nodes WHERE agent_id = ?1 AND source_session_id = ?2 AND node_type = 'topic' ORDER BY start_log_id ASC",
    )?;
    let rows = stmt.query_map(params![agent_id, session_id], |row| {
        Ok(IndexNodeRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            parent_id: row.get(2)?,
            node_type: row.get(3)?,
            source_type: row.get(4)?,
            title: row.get(5)?,
            summary: row.get(6)?,
            start_log_id: row.get(7)?,
            end_log_id: row.get(8)?,
            source_session_id: row.get(9)?,
            date_from: row.get(10)?,
            date_to: row.get(11)?,
            depth: row.get(12)?,
            child_count: row.get(13)?,
            token_count: row.get(14)?,
            created_at: row.get(15)?,
            updated_at: row.get(16)?,
            short_id: row.get(17)?,
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
    pub permission: String,
    pub archived: bool,
}

pub fn list_skills(conn: &Connection, agent_id: &str, active_only: bool) -> Result<Vec<SkillRow>> {
    list_skills_filtered(conn, agent_id, active_only, false)
}

pub fn list_skills_filtered(
    conn: &Connection,
    agent_id: &str,
    active_only: bool,
    include_archived: bool,
) -> Result<Vec<SkillRow>> {
    let sql = match (active_only, include_archived) {
        (true, _) => {
            "SELECT id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, permission, archived
             FROM skills WHERE agent_id = ?1 AND is_active = 1 AND archived = 0 ORDER BY usage_count DESC"
        }
        (false, true) => {
            "SELECT id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, permission, archived
             FROM skills WHERE agent_id = ?1 ORDER BY usage_count DESC"
        }
        (false, false) => {
            "SELECT id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, permission, archived
             FROM skills WHERE agent_id = ?1 AND archived = 0 ORDER BY usage_count DESC"
        }
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
            permission: row.get(12)?,
            archived: row.get(13)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn insert_skill(conn: &Connection, skill: &SkillRow) -> Result<()> {
    conn.execute(
        "INSERT INTO skills (id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, permission, archived, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
            skill.permission,
            skill.archived,
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

pub fn find_skill_by_name(
    conn: &Connection,
    agent_id: &str,
    name: &str,
) -> Result<Option<SkillRow>> {
    let result = conn.query_row(
        "SELECT id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, permission, archived
         FROM skills WHERE agent_id = ?1 AND LOWER(name) = LOWER(?2) AND archived = 0 LIMIT 1",
        params![agent_id, name],
        |row| {
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
                permission: row.get(12)?,
                archived: row.get(13)?,
            })
        },
    );

    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn find_skill_by_name_any(
    conn: &Connection,
    agent_id: &str,
    name: &str,
) -> Result<Option<SkillRow>> {
    let result = conn.query_row(
        "SELECT id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, permission, archived
         FROM skills WHERE agent_id = ?1 AND LOWER(name) = LOWER(?2) LIMIT 1",
        params![agent_id, name],
        |row| {
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
                permission: row.get(12)?,
                archived: row.get(13)?,
            })
        },
    );

    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn find_skill_by_id(conn: &Connection, skill_id: &str) -> Result<Option<SkillRow>> {
    let result = conn.query_row(
        "SELECT id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, permission, archived
         FROM skills WHERE id = ?1 LIMIT 1",
        params![skill_id],
        |row| {
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
                permission: row.get(12)?,
                archived: row.get(13)?,
            })
        },
    );

    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn update_skill(conn: &Connection, skill: &SkillRow) -> Result<()> {
    conn.execute(
        "UPDATE skills SET name = ?1, description = ?2, situation_pattern = ?3, guidance = ?4, is_active = ?5, archived = ?6, file_path = ?7, updated_at = ?8 WHERE id = ?9",
        params![
            skill.name,
            skill.description,
            skill.situation_pattern,
            skill.guidance,
            skill.is_active,
            skill.archived,
            skill.file_path,
            Utc::now().to_rfc3339(),
            skill.id,
        ],
    )?;
    Ok(())
}

pub fn archive_skill(conn: &Connection, skill_id: &str, archived: bool) -> Result<()> {
    conn.execute(
        "UPDATE skills SET archived = ?1, updated_at = ?2 WHERE id = ?3",
        params![archived, Utc::now().to_rfc3339(), skill_id],
    )?;
    Ok(())
}

pub fn find_unused_skills(
    conn: &Connection,
    agent_id: &str,
    days_old: i64,
) -> Result<Vec<SkillRow>> {
    let cutoff = (Utc::now() - chrono::Duration::days(days_old)).to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, permission, archived
         FROM skills
         WHERE agent_id = ?1 AND usage_count = 0 AND archived = 0 AND created_at <= ?2
         ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![agent_id, cutoff], |row| {
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
            permission: row.get(12)?,
            archived: row.get(13)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
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
    pub metadata_json: Option<String>,
}

pub fn insert_session(conn: &Connection, session: &SessionRow) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (id, mode, theme, phase, turn_number, status, participant_ids_json, facilitator_id, done_count, max_turns, metadata_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
            session.metadata_json,
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn get_session(conn: &Connection, session_id: &str) -> Result<Option<SessionRow>> {
    let result = conn.query_row(
        "SELECT id, mode, theme, phase, turn_number, status, participant_ids_json, facilitator_id, done_count, max_turns, metadata_json
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
                metadata_json: row.get(10)?,
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
        "SELECT id, mode, theme, phase, turn_number, status, participant_ids_json, facilitator_id, done_count, max_turns, metadata_json
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
            metadata_json: row.get(10)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn update_session_metadata(
    conn: &Connection,
    session_id: &str,
    metadata_json: &str,
    theme: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET metadata_json = ?1, theme = ?2, updated_at = ?3 WHERE id = ?4",
        params![metadata_json, theme, Utc::now().to_rfc3339(), session_id],
    )?;
    Ok(())
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
    pub whitelisted: bool,
    pub heartbeat_enabled: bool,
    pub heartbeat_interval_secs: Option<u64>,
}

pub fn get_channel_config(conn: &Connection, channel_id: &str) -> Result<Option<ChannelConfigRow>> {
    let result = conn.query_row(
        "SELECT channel_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs
         FROM discord_channel_config WHERE channel_id = ?1",
        params![channel_id],
        |row| {
            Ok(ChannelConfigRow {
                channel_id: row.get(0)?,
                guild_id: row.get(1)?,
                channel_name: row.get(2)?,
                readable: row.get(3)?,
                writable: row.get(4)?,
                whitelisted: row.get(5)?,
                heartbeat_enabled: row.get(6)?,
                heartbeat_interval_secs: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
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
        "INSERT INTO discord_channel_config (channel_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(channel_id) DO UPDATE SET
            guild_id = excluded.guild_id,
            channel_name = excluded.channel_name,
            readable = excluded.readable,
            writable = excluded.writable,
            whitelisted = excluded.whitelisted,
            heartbeat_enabled = excluded.heartbeat_enabled,
            heartbeat_interval_secs = excluded.heartbeat_interval_secs,
            updated_at = excluded.updated_at",
        params![
            cfg.channel_id,
            cfg.guild_id,
            cfg.channel_name,
            cfg.readable,
            cfg.writable,
            cfg.whitelisted,
            cfg.heartbeat_enabled,
            cfg.heartbeat_interval_secs.map(|v| v as i64),
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

pub fn delete_channel_config(conn: &Connection, channel_id: &str) -> Result<bool> {
    let rows_affected = conn.execute(
        "DELETE FROM discord_channel_config WHERE channel_id = ?1",
        rusqlite::params![channel_id],
    )?;
    Ok(rows_affected > 0)
}

pub fn list_channel_configs_by_guild(
    conn: &Connection,
    guild_id: &str,
) -> Result<Vec<ChannelConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT channel_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs
         FROM discord_channel_config WHERE guild_id = ?1 ORDER BY channel_name",
    )?;

    let rows = stmt.query_map(params![guild_id], |row| {
        Ok(ChannelConfigRow {
            channel_id: row.get(0)?,
            guild_id: row.get(1)?,
            channel_name: row.get(2)?,
            readable: row.get(3)?,
            writable: row.get(4)?,
            whitelisted: row.get(5)?,
            heartbeat_enabled: row.get(6)?,
            heartbeat_interval_secs: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// whitelisted=true のチャンネルをすべて取得する。
pub fn list_whitelisted_channels(conn: &Connection) -> Result<Vec<ChannelConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT channel_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs
         FROM discord_channel_config WHERE whitelisted = 1 ORDER BY channel_id",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(ChannelConfigRow {
            channel_id: row.get(0)?,
            guild_id: row.get(1)?,
            channel_name: row.get(2)?,
            readable: row.get(3)?,
            writable: row.get(4)?,
            whitelisted: row.get(5)?,
            heartbeat_enabled: row.get(6)?,
            heartbeat_interval_secs: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// heartbeat_enabled=true のチャンネルをすべて取得する。
/// ハートビートを有効にすべきチャンネル一覧。
pub fn list_heartbeat_channels(conn: &Connection) -> Result<Vec<ChannelConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT channel_id, guild_id, channel_name, readable, writable, whitelisted, heartbeat_enabled, heartbeat_interval_secs
         FROM discord_channel_config WHERE heartbeat_enabled = 1 ORDER BY channel_id",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(ChannelConfigRow {
            channel_id: row.get(0)?,
            guild_id: row.get(1)?,
            channel_name: row.get(2)?,
            readable: row.get(3)?,
            writable: row.get(4)?,
            whitelisted: row.get(5)?,
            heartbeat_enabled: row.get(6)?,
            heartbeat_interval_secs: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
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

pub fn patch_agent_discord_config(
    conn: &Connection,
    agent_id: &str,
    bot_token: Option<&str>,
    owner_discord_id: Option<&str>,
) -> Result<bool> {
    let updated = match (bot_token, owner_discord_id) {
        (Some(token), Some(owner)) => conn.execute(
            "UPDATE agent_discord_config SET bot_token = ?1, owner_discord_id = ?2, updated_at = ?3 WHERE agent_id = ?4",
            params![token, owner, chrono::Utc::now().to_rfc3339(), agent_id],
        )?,
        (Some(token), None) => conn.execute(
            "UPDATE agent_discord_config SET bot_token = ?1, updated_at = ?2 WHERE agent_id = ?3",
            params![token, chrono::Utc::now().to_rfc3339(), agent_id],
        )?,
        (None, Some(owner)) => conn.execute(
            "UPDATE agent_discord_config SET owner_discord_id = ?1, updated_at = ?2 WHERE agent_id = ?3",
            params![owner, chrono::Utc::now().to_rfc3339(), agent_id],
        )?,
        (None, None) => 0,
    };
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

/// チャンネルがホワイトリストに登録されているか判定する。設定なし=false（デフォルト拒否）。
pub fn is_channel_whitelisted(conn: &Connection, channel_id: &str) -> bool {
    get_channel_config(conn, channel_id)
        .ok()
        .flatten()
        .map(|c| c.whitelisted)
        .unwrap_or(false)
}

// ============================================
// MEMORY INDEX: 階層ツリーノード
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexNodeRow {
    pub id: String,
    pub agent_id: String,
    pub parent_id: Option<String>,
    pub node_type: String,
    pub source_type: String,
    pub title: String,
    pub summary: String,
    pub start_log_id: Option<i64>,
    pub end_log_id: Option<i64>,
    pub source_session_id: Option<String>,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub depth: i32,
    pub child_count: i32,
    pub token_count: i32,
    pub created_at: String,
    pub updated_at: String,
    pub short_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatermarkRow {
    pub agent_id: String,
    pub last_indexed_log_id: i64,
    pub last_indexed_at: String,
    pub total_nodes: i64,
}

#[derive(Debug, Clone)]
pub struct DailyLogWatermarkRow {
    pub agent_id: String,
    pub last_indexed_date: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct DailyLogEntry {
    pub id: String,
    pub agent_id: String,
    pub category: String,
    pub content: String,
    pub date_str: String,
}

pub fn insert_index_node(conn: &Connection, node: &IndexNodeRow) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_index_nodes (id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id, end_log_id, source_session_id, date_from, date_to, depth, child_count, token_count, created_at, updated_at, short_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            node.id,
            node.agent_id,
            node.parent_id,
            node.node_type,
            node.source_type,
            node.title,
            node.summary,
            node.start_log_id,
            node.end_log_id,
            node.source_session_id,
            node.date_from,
            node.date_to,
            node.depth,
            node.child_count,
            node.token_count,
            node.created_at,
            node.updated_at,
            node.short_id,
        ],
    )?;
    Ok(())
}

pub fn update_index_node_child_count(conn: &Connection, node_id: &str, count: i32) -> Result<()> {
    conn.execute(
        "UPDATE memory_index_nodes SET child_count = ?1, updated_at = ?2 WHERE id = ?3",
        params![count, Utc::now().to_rfc3339(), node_id],
    )?;
    Ok(())
}

pub fn get_index_tree(conn: &Connection, agent_id: &str) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id, end_log_id, source_session_id, date_from, date_to, depth, child_count, token_count, created_at, updated_at, short_id
         FROM memory_index_nodes WHERE agent_id = ?1 ORDER BY depth ASC, created_at ASC",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| {
        Ok(IndexNodeRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            parent_id: row.get(2)?,
            node_type: row.get(3)?,
            source_type: row.get(4)?,
            title: row.get(5)?,
            summary: row.get(6)?,
            start_log_id: row.get(7)?,
            end_log_id: row.get(8)?,
            source_session_id: row.get(9)?,
            date_from: row.get(10)?,
            date_to: row.get(11)?,
            depth: row.get(12)?,
            child_count: row.get(13)?,
            token_count: row.get(14)?,
            created_at: row.get(15)?,
            updated_at: row.get(16)?,
            short_id: row.get(17)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn get_index_node(conn: &Connection, node_id: &str) -> Result<Option<IndexNodeRow>> {
    let result = conn.query_row(
        "SELECT id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id, end_log_id, source_session_id, date_from, date_to, depth, child_count, token_count, created_at, updated_at, short_id
         FROM memory_index_nodes WHERE id = ?1",
        params![node_id],
        |row| {
            Ok(IndexNodeRow {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                parent_id: row.get(2)?,
                node_type: row.get(3)?,
                source_type: row.get(4)?,
                title: row.get(5)?,
                summary: row.get(6)?,
                start_log_id: row.get(7)?,
                end_log_id: row.get(8)?,
                source_session_id: row.get(9)?,
                date_from: row.get(10)?,
                date_to: row.get(11)?,
                depth: row.get(12)?,
                child_count: row.get(13)?,
                token_count: row.get(14)?,
                created_at: row.get(15)?,
                updated_at: row.get(16)?,
                short_id: row.get(17)?,
            })
        },
    );
    match result {
        Ok(node) => Ok(Some(node)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn get_daily_log_watermark(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<DailyLogWatermarkRow>> {
    let result = conn.query_row(
        "SELECT agent_id, last_indexed_date, updated_at
         FROM daily_log_index_watermark WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(DailyLogWatermarkRow {
                agent_id: row.get(0)?,
                last_indexed_date: row.get(1)?,
                updated_at: row.get(2)?,
            })
        },
    );

    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_daily_log_watermark(conn: &Connection, row: &DailyLogWatermarkRow) -> Result<()> {
    conn.execute(
        "INSERT INTO daily_log_index_watermark (agent_id, last_indexed_date, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(agent_id) DO UPDATE SET
            last_indexed_date = excluded.last_indexed_date,
            updated_at = excluded.updated_at",
        params![row.agent_id, row.last_indexed_date, row.updated_at],
    )?;
    Ok(())
}

pub fn delete_daily_log_watermark(conn: &Connection, agent_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM daily_log_index_watermark WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(())
}

pub fn get_unindexed_daily_logs(
    conn: &Connection,
    agent_id: &str,
    after_date: &str,
) -> Result<Vec<DailyLogEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, category, content
         FROM memory_curated
         WHERE agent_id = ?1
           AND category LIKE 'daily_log/%'
           AND substr(category, 11) > ?2
         ORDER BY category ASC",
    )?;
    let rows = stmt.query_map(params![agent_id, after_date], |row| {
        let category: String = row.get(2)?;
        Ok(DailyLogEntry {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            date_str: category.trim_start_matches("daily_log/").to_string(),
            category,
            content: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn get_daily_log_by_date(
    conn: &Connection,
    agent_id: &str,
    date_str: &str,
) -> Result<Option<DailyLogEntry>> {
    let category = format!("daily_log/{date_str}");
    let result = conn.query_row(
        "SELECT id, agent_id, category, content
         FROM memory_curated
         WHERE agent_id = ?1 AND category = ?2",
        params![agent_id, category],
        |row| {
            let category: String = row.get(2)?;
            Ok(DailyLogEntry {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                date_str: category.trim_start_matches("daily_log/").to_string(),
                category,
                content: row.get(3)?,
            })
        },
    );

    match result {
        Ok(entry) => Ok(Some(entry)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_daily_log_index_node(conn: &Connection, node: &IndexNodeRow) -> Result<()> {
    conn.execute(
        "INSERT INTO memory_index_nodes (id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id, end_log_id, source_session_id, date_from, date_to, depth, child_count, token_count, created_at, updated_at, short_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
         ON CONFLICT(id) DO UPDATE SET
            title = excluded.title,
            summary = excluded.summary,
            updated_at = excluded.updated_at,
            child_count = excluded.child_count",
        params![
            node.id,
            node.agent_id,
            node.parent_id,
            node.node_type,
            node.source_type,
            node.title,
            node.summary,
            node.start_log_id,
            node.end_log_id,
            node.source_session_id,
            node.date_from,
            node.date_to,
            node.depth,
            node.child_count,
            node.token_count,
            node.created_at,
            node.updated_at,
            node.short_id,
        ],
    )?;
    Ok(())
}

pub fn get_session_logs_by_id_range(
    conn: &Connection,
    agent_id: &str,
    from_id: i64,
    to_id: i64,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE agent_id = ?1 AND id >= ?2 AND id <= ?3 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![agent_id, from_id, to_id], |row| {
        Ok(SessionLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            log_type: row.get(3)?,
            content: row.get(4)?,
            speaker_id: row.get(5)?,
            turn_number: row.get(6)?,
            metadata_json: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn get_index_watermark(conn: &Connection, agent_id: &str) -> Result<Option<WatermarkRow>> {
    let result = conn.query_row(
        "SELECT agent_id, last_indexed_log_id, last_indexed_at, total_nodes
         FROM memory_index_watermark WHERE agent_id = ?1",
        params![agent_id],
        |row| {
            Ok(WatermarkRow {
                agent_id: row.get(0)?,
                last_indexed_log_id: row.get(1)?,
                last_indexed_at: row.get(2)?,
                total_nodes: row.get(3)?,
            })
        },
    );
    match result {
        Ok(wm) => Ok(Some(wm)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_index_watermark(conn: &Connection, wm: &WatermarkRow) -> Result<()> {
    conn.execute(
        "INSERT INTO memory_index_watermark (agent_id, last_indexed_log_id, last_indexed_at, total_nodes)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent_id) DO UPDATE SET
            last_indexed_log_id = excluded.last_indexed_log_id,
            last_indexed_at = excluded.last_indexed_at,
            total_nodes = excluded.total_nodes",
        params![wm.agent_id, wm.last_indexed_log_id, wm.last_indexed_at, wm.total_nodes],
    )?;
    Ok(())
}

pub fn get_unindexed_log_count(conn: &Connection, agent_id: &str) -> Result<i64> {
    let last_id = get_index_watermark(conn, agent_id)?
        .map(|wm| wm.last_indexed_log_id)
        .unwrap_or(0);
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_sessions WHERE agent_id = ?1 AND id > ?2",
        params![agent_id, last_id],
        |row| row.get(0),
    )?;
    Ok(count)
}

pub fn get_unindexed_session_logs(
    conn: &Connection,
    agent_id: &str,
    after_id: i64,
    limit: usize,
) -> Result<Vec<SessionLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, log_type, content, speaker_id, turn_number, metadata_json, created_at
         FROM memory_sessions WHERE agent_id = ?1 AND id > ?2 ORDER BY id ASC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![agent_id, after_id, limit as i64], |row| {
        Ok(SessionLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            log_type: row.get(3)?,
            content: row.get(4)?,
            speaker_id: row.get(5)?,
            turn_number: row.get(6)?,
            metadata_json: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// エージェントの全インデックスノードを削除する
pub fn delete_index_nodes_for_agent(conn: &Connection, agent_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_index_nodes WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(())
}

/// エージェントのインデックスウォーターマークを削除する
pub fn delete_index_watermark_for_agent(conn: &Connection, agent_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_index_watermark WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(())
}

/// インデックスノードのtitle/summaryを更新する（再マージ用）
pub fn update_index_node_summary(
    conn: &Connection,
    node_id: &str,
    title: &str,
    summary: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE memory_index_nodes SET title = ?1, summary = ?2, updated_at = ?3 WHERE id = ?4",
        params![title, summary, Utc::now().to_rfc3339(), node_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        crate::init_memory().expect("failed to init in-memory DB")
    }

    #[test]
    fn test_agent_upsert_and_get() {
        let conn = setup();
        let agent = AgentRow {
            agent_id: "agent-1".to_string(),
            name: "Alice".to_string(),
            job_title: Some("Engineer".to_string()),
            organization: Some("OpenCrab Inc.".to_string()),
            image_url: Some("https://example.com/avatar.png".to_string()),
            persona_name: "Crab".to_string(),
            personality: Some(r#"{"hobby":"coding"}"#.to_string()),
            instructions: String::new(),
            model: None,
            metadata_json: Some(r#"{"lang":"en"}"#.to_string()),
        };

        upsert_agent(&conn, &agent).unwrap();

        let fetched = get_agent(&conn, "agent-1").unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.agent_id, "agent-1");
        assert_eq!(fetched.name, "Alice");
        assert_eq!(fetched.persona_name, "Crab");
        assert_eq!(
            fetched.personality,
            Some(r#"{"hobby":"coding"}"#.to_string())
        );
        assert_eq!(fetched.job_title, Some("Engineer".to_string()));
        assert_eq!(
            fetched.image_url,
            Some("https://example.com/avatar.png".to_string())
        );
        assert_eq!(fetched.metadata_json, Some(r#"{"lang":"en"}"#.to_string()));
    }

    #[test]
    fn test_agent_get_nonexistent() {
        let conn = setup();
        let result = get_agent(&conn, "nonexistent-agent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_effective_model_for_agent() {
        let conn = setup();
        let agent = AgentRow {
            agent_id: "a1".to_string(),
            name: "N".to_string(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".to_string(),
            personality: None,
            instructions: String::new(),
            model: Some("openai:gpt-4o".to_string()),
            metadata_json: None,
        };
        upsert_agent(&conn, &agent).unwrap();
        let m = effective_model_for_agent(&conn, "a1", "anthropic:claude").unwrap();
        assert_eq!(m, "openai:gpt-4o");
        let m2 = effective_model_for_agent(&conn, "a1", "anthropic:claude").unwrap();
        assert_eq!(m2, "openai:gpt-4o");

        let agent2 = AgentRow {
            agent_id: "a2".to_string(),
            name: "N2".to_string(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".to_string(),
            personality: None,
            instructions: String::new(),
            model: None,
            metadata_json: None,
        };
        upsert_agent(&conn, &agent2).unwrap();
        let m3 = effective_model_for_agent(&conn, "a2", "global:default").unwrap();
        assert_eq!(m3, "global:default");
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
            created_at: String::new(),
        };
        let mem2 = CuratedMemoryRow {
            id: "mem-2".to_string(),
            agent_id: "agent-1".to_string(),
            category: "facts".to_string(),
            content: "Crabs have ten legs.".to_string(),
            created_at: String::new(),
        };

        upsert_curated_memory(&conn, &mem1).unwrap();
        upsert_curated_memory(&conn, &mem2).unwrap();

        let results = get_curated_memories(&conn, "agent-1", "facts").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "Crabs have ten legs.");
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
            created_at: String::new(),
        };
        let mem2 = CuratedMemoryRow {
            id: "mem-2".to_string(),
            agent_id: "agent-1".to_string(),
            category: "opinions".to_string(),
            content: "Rust is great.".to_string(),
            created_at: String::new(),
        };

        upsert_curated_memory(&conn, &mem1).unwrap();
        upsert_curated_memory(&conn, &mem2).unwrap();

        let (all, _total) = list_curated_memories(&conn, "agent-1", 10000, 0).unwrap();
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
            created_at: None,
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
            created_at: None,
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
            created_at: None,
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
            created_at: None,
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
            created_at: None,
        };

        insert_session_log(&conn, &log1).unwrap();
        insert_session_log(&conn, &log2).unwrap();

        let results = search_session_logs(&conn, "agent-1", "quantum cryptography", 10).unwrap();
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
            created_at: None,
        };
        insert_session_log(&conn, &log).unwrap();

        let results = search_session_logs(&conn, "agent-1", "nonexistenttermxyz", 10).unwrap();
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
            permission: "\"agent\"".to_string(),
            archived: false,
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
            permission: "\"agent\"".to_string(),
            archived: false,
        };

        insert_skill(&conn, &skill).unwrap();
        increment_skill_usage(&conn, "skill-1").unwrap();

        let skills = list_skills(&conn, "agent-1", true).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].usage_count, 1);
    }

    // 11a. test_find_skill_by_name_any_includes_archived
    #[test]
    fn test_find_skill_by_name_any_includes_archived() {
        let conn = setup();

        let skill = SkillRow {
            id: "skill-arch-1".to_string(),
            agent_id: "agent-1".to_string(),
            name: "ArchivedSkill".to_string(),
            description: "Some description".to_string(),
            situation_pattern: "".to_string(),
            guidance: "".to_string(),
            source_type: "acquired".to_string(),
            source_context: None,
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: false,
            permission: "\"agent\"".to_string(),
            archived: true,
        };
        insert_skill(&conn, &skill).unwrap();

        // find_skill_by_name should NOT find archived
        let not_found = find_skill_by_name(&conn, "agent-1", "ArchivedSkill").unwrap();
        assert!(
            not_found.is_none(),
            "find_skill_by_name should not find archived skill"
        );

        // find_skill_by_name_any SHOULD find archived
        let found = find_skill_by_name_any(&conn, "agent-1", "ArchivedSkill").unwrap();
        assert!(
            found.is_some(),
            "find_skill_by_name_any should find archived skill"
        );
        assert_eq!(found.unwrap().archived, true);
    }

    // 11b. test_update_skill_full_fields
    #[test]
    fn test_update_skill_full_fields() {
        let conn = setup();

        let skill = SkillRow {
            id: "skill-upd-1".to_string(),
            agent_id: "agent-1".to_string(),
            name: "UpdateMe".to_string(),
            description: "Original description".to_string(),
            situation_pattern: "original pattern".to_string(),
            guidance: "original guidance".to_string(),
            source_type: "acquired".to_string(),
            source_context: None,
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: "\"agent\"".to_string(),
            archived: true,
        };
        insert_skill(&conn, &skill).unwrap();

        // Update with new values including archived=false restore
        let mut updated = skill.clone();
        updated.description = "Updated description".to_string();
        updated.guidance = "Updated guidance".to_string();
        updated.archived = false;
        updated.is_active = true;
        update_skill(&conn, &updated).unwrap();

        let found = find_skill_by_name(&conn, "agent-1", "UpdateMe").unwrap();
        assert!(found.is_some(), "should find restored skill");
        let s = found.unwrap();
        assert_eq!(s.description, "Updated description");
        assert_eq!(s.guidance, "Updated guidance");
        assert_eq!(s.archived, false);
        assert_eq!(s.is_active, true);
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
            metadata_json: None,
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

        let summary = get_llm_metrics_summary(&conn, "agent-1", "2020-01-01").unwrap();
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
        let gpt4o_conv = stats
            .iter()
            .find(|s| s.model == "gpt-4o" && s.purpose == "conversation")
            .unwrap();
        let gpt4o_anl = stats
            .iter()
            .find(|s| s.model == "gpt-4o" && s.purpose == "analysis")
            .unwrap();
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

        let result = insert_heartbeat_log(&conn, "agent-1", "idle", Some(r#"{"action":"none"}"#));
        assert!(result.is_ok());
    }

    // ── delete_agent ──

    #[test]
    fn test_delete_agent() {
        let conn = setup();

        upsert_agent(
            &conn,
            &AgentRow {
                agent_id: "del-1".into(),
                name: "DeleteMe".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "Doomed".into(),
                personality: None,
                instructions: String::new(),
                model: None,
                metadata_json: None,
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
                created_at: String::new(),
            },
        )
        .unwrap();

        assert!(get_agent(&conn, "del-1").unwrap().is_some());

        let deleted = delete_agent(&conn, "del-1").unwrap();
        assert!(deleted);

        assert!(get_agent(&conn, "del-1").unwrap().is_none());
        assert!(list_curated_memories(&conn, "del-1", 10000, 0)
            .unwrap()
            .0
            .is_empty());
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
        upsert_agent(
            &conn,
            &AgentRow {
                agent_id: "abc-12345".into(),
                name: "Alice".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "a".into(),
                personality: None,
                instructions: String::new(),
                model: None,
                metadata_json: None,
            },
        )
        .unwrap();
        upsert_agent(
            &conn,
            &AgentRow {
                agent_id: "xyz-99999".into(),
                name: "Bob".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "b".into(),
                personality: None,
                instructions: String::new(),
                model: None,
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
        upsert_agent(
            &conn,
            &AgentRow {
                agent_id: "agent-find-1".into(),
                name: "Creative Researcher".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "cr".into(),
                personality: None,
                instructions: String::new(),
                model: None,
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

        let agent_id = "crud-agent-1";
        upsert_agent(
            &conn,
            &AgentRow {
                agent_id: agent_id.into(),
                name: "TestAgent".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "Original Persona".into(),
                personality: None,
                instructions: String::new(),
                model: None,
                metadata_json: None,
            },
        )
        .unwrap();

        let row = get_agent(&conn, agent_id).unwrap().unwrap();
        assert_eq!(row.name, "TestAgent");
        assert_eq!(row.persona_name, "Original Persona");

        upsert_agent(
            &conn,
            &AgentRow {
                agent_id: agent_id.into(),
                name: "UpdatedAgent".into(),
                job_title: Some("Lead".into()),
                organization: None,
                image_url: None,
                persona_name: "Updated Persona".into(),
                personality: None,
                instructions: String::new(),
                model: None,
                metadata_json: None,
            },
        )
        .unwrap();

        let row = get_agent(&conn, agent_id).unwrap().unwrap();
        assert_eq!(row.name, "UpdatedAgent");
        assert_eq!(row.job_title, Some("Lead".to_string()));
        assert_eq!(row.persona_name, "Updated Persona");

        // Find
        let results = find_agents(&conn, "Updated").unwrap();
        assert_eq!(results.len(), 1);

        // Delete
        let deleted = delete_agent(&conn, agent_id).unwrap();
        assert!(deleted);
        assert!(get_agent(&conn, agent_id).unwrap().is_none());

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
            whitelisted: false,
            heartbeat_enabled: true,
            heartbeat_interval_secs: None,
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
            whitelisted: false,
            heartbeat_enabled: true,
            heartbeat_interval_secs: None,
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
            whitelisted: false,
            heartbeat_enabled: true,
            heartbeat_interval_secs: None,
        };
        let cfg2 = ChannelConfigRow {
            channel_id: "ch-2".to_string(),
            guild_id: "guild-1".to_string(),
            channel_name: "random".to_string(),
            readable: false,
            writable: true,
            whitelisted: false,
            heartbeat_enabled: true,
            heartbeat_interval_secs: None,
        };
        let cfg3 = ChannelConfigRow {
            channel_id: "ch-3".to_string(),
            guild_id: "guild-2".to_string(),
            channel_name: "other".to_string(),
            readable: true,
            writable: true,
            whitelisted: false,
            heartbeat_enabled: true,
            heartbeat_interval_secs: None,
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
            whitelisted: false,
            heartbeat_enabled: true,
            heartbeat_interval_secs: None,
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
        assert!(get_agent_discord_config(&conn, "agent-del")
            .unwrap()
            .is_some());

        let deleted = delete_agent_discord_config(&conn, "agent-del").unwrap();
        assert!(deleted);
        assert!(get_agent_discord_config(&conn, "agent-del")
            .unwrap()
            .is_none());

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
        let fetched = get_agent_discord_config(&conn, "agent-toggle")
            .unwrap()
            .unwrap();
        assert!(fetched.enabled);

        // Disable
        let updated = set_agent_discord_config_enabled(&conn, "agent-toggle", false).unwrap();
        assert!(updated);
        let fetched = get_agent_discord_config(&conn, "agent-toggle")
            .unwrap()
            .unwrap();
        assert!(!fetched.enabled);

        // Re-enable
        let updated = set_agent_discord_config_enabled(&conn, "agent-toggle", true).unwrap();
        assert!(updated);
        let fetched = get_agent_discord_config(&conn, "agent-toggle")
            .unwrap()
            .unwrap();
        assert!(fetched.enabled);

        // Nonexistent agent → false
        let updated = set_agent_discord_config_enabled(&conn, "no-such", false).unwrap();
        assert!(!updated);
    }

    #[test]
    fn test_delete_agent_also_removes_discord_config() {
        let conn = setup();

        let agent_id = "agent-discord-del";
        upsert_agent(
            &conn,
            &AgentRow {
                agent_id: agent_id.into(),
                name: "DiscordAgent".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "d".into(),
                personality: None,
                instructions: String::new(),
                model: None,
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

    // ============================================
    // short_id tests (T-1.1 ~ T-1.6)
    // ============================================

    #[test]
    fn test_next_short_id_empty_table() {
        // T-1.1: Empty table should return "t1"
        let conn = setup();
        let result = next_short_id(&conn, "a1", "t").unwrap();
        assert_eq!(result, "t1");
    }

    #[test]
    fn test_next_short_id_sequential() {
        // T-1.2: With t1,t2,t3 existing, should return "t4"
        let conn = setup();
        for i in 1..=3 {
            insert_index_node(&conn, &IndexNodeRow {
                id: format!("node-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: format!("Topic {i}"),
                summary: "test".to_string(),
                start_log_id: None,
                end_log_id: None,
                source_session_id: None,
                date_from: None,
                date_to: None,
                depth: 0,
                child_count: 0,
                token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{i}")),
            }).unwrap();
        }
        let result = next_short_id(&conn, "a1", "t").unwrap();
        assert_eq!(result, "t4");
    }

    #[test]
    fn test_next_short_id_independent_prefix() {
        // T-1.3: t1, t2, h1 exist -> prefix="h" returns "h2"
        let conn = setup();
        for (id, prefix, num) in &[("n1", "t", 1), ("n2", "t", 2), ("n3", "h", 1)] {
            insert_index_node(&conn, &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None,
                node_type: "topic".to_string(),
                source_type: String::new(),
                title: "T".to_string(),
                summary: "s".to_string(),
                start_log_id: None, end_log_id: None, source_session_id: None,
                date_from: None, date_to: None,
                depth: 0, child_count: 0, token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("{prefix}{num}")),
            }).unwrap();
        }
        let result = next_short_id(&conn, "a1", "h").unwrap();
        assert_eq!(result, "h2");
    }

    #[test]
    fn test_next_short_id_independent_agent() {
        // T-1.4: agent a1 has t1-t10, agent a2 has t1 -> a2 prefix="t" returns "t2"
        let conn = setup();
        for i in 1..=10 {
            insert_index_node(&conn, &IndexNodeRow {
                id: format!("a1-node-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None, node_type: "topic".to_string(), source_type: String::new(),
                title: "T".to_string(), summary: "s".to_string(),
                start_log_id: None, end_log_id: None, source_session_id: None,
                date_from: None, date_to: None,
                depth: 0, child_count: 0, token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{i}")),
            }).unwrap();
        }
        insert_index_node(&conn, &IndexNodeRow {
            id: "a2-node-1".to_string(),
            agent_id: "a2".to_string(),
            parent_id: None, node_type: "topic".to_string(), source_type: String::new(),
            title: "T".to_string(), summary: "s".to_string(),
            start_log_id: None, end_log_id: None, source_session_id: None,
            date_from: None, date_to: None,
            depth: 0, child_count: 0, token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t1".to_string()),
        }).unwrap();
        let result = next_short_id(&conn, "a2", "t").unwrap();
        assert_eq!(result, "t2");
    }

    #[test]
    fn test_next_short_id_with_gaps() {
        // T-1.5: t1, t3, t5 exist (gaps) -> returns "t6" (MAX+1)
        let conn = setup();
        for (id, num) in &[("n1", 1), ("n2", 3), ("n3", 5)] {
            insert_index_node(&conn, &IndexNodeRow {
                id: id.to_string(),
                agent_id: "a1".to_string(),
                parent_id: None, node_type: "topic".to_string(), source_type: String::new(),
                title: "T".to_string(), summary: "s".to_string(),
                start_log_id: None, end_log_id: None, source_session_id: None,
                date_from: None, date_to: None,
                depth: 0, child_count: 0, token_count: 0,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{num}")),
            }).unwrap();
        }
        let result = next_short_id(&conn, "a1", "t").unwrap();
        assert_eq!(result, "t6");
    }

    #[test]
    fn test_next_short_id_all_prefixes() {
        // T-1.6: All prefix patterns return "{prefix}1" on empty table
        let conn = setup();
        for prefix in &["t", "h", "d", "w", "m", "y", "p", "r", "s"] {
            let result = next_short_id(&conn, "a1", prefix).unwrap();
            assert_eq!(result, format!("{prefix}1"), "Failed for prefix {prefix}");
        }
    }

    // ============================================
    // backfill_short_ids tests (T-1.7 ~ T-1.9)
    // ============================================

    #[test]
    fn test_backfill_short_ids_basic() {
        // T-1.7: 5 topics + 3 dailies with NULL short_id -> get assigned
        let conn = setup();
        for i in 1..=5 {
            insert_index_node(&conn, &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None, node_type: "topic".to_string(), source_type: String::new(),
                title: format!("Topic {i}"), summary: "s".to_string(),
                start_log_id: None, end_log_id: None, source_session_id: None,
                date_from: None, date_to: None,
                depth: 0, child_count: 0, token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
            }).unwrap();
        }
        for i in 1..=3 {
            insert_index_node(&conn, &IndexNodeRow {
                id: format!("daily-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None, node_type: "daily".to_string(), source_type: String::new(),
                title: format!("Daily {i}"), summary: "s".to_string(),
                start_log_id: None, end_log_id: None, source_session_id: None,
                date_from: None, date_to: None,
                depth: 0, child_count: 0, token_count: 0,
                created_at: format!("2026-01-01T01:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
            }).unwrap();
        }
        let count = backfill_short_ids(&conn).unwrap();
        assert_eq!(count, 8);
        // Verify topics got t1-t5, dailies got d1-d3
        let node = get_index_node(&conn, "topic-1").unwrap().unwrap();
        assert_eq!(node.short_id, Some("t1".to_string()));
        let node = get_index_node(&conn, "topic-5").unwrap().unwrap();
        assert_eq!(node.short_id, Some("t5".to_string()));
        let node = get_index_node(&conn, "daily-1").unwrap().unwrap();
        assert_eq!(node.short_id, Some("d1".to_string()));
        let node = get_index_node(&conn, "daily-3").unwrap().unwrap();
        assert_eq!(node.short_id, Some("d3".to_string()));
    }

    #[test]
    fn test_backfill_short_ids_skip_existing() {
        // T-1.8: t1, t2 already set, 3 NULL -> only NULL ones get t3, t4, t5
        let conn = setup();
        for i in 1..=2 {
            insert_index_node(&conn, &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None, node_type: "topic".to_string(), source_type: String::new(),
                title: "T".to_string(), summary: "s".to_string(),
                start_log_id: None, end_log_id: None, source_session_id: None,
                date_from: None, date_to: None,
                depth: 0, child_count: 0, token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: Some(format!("t{i}")),
            }).unwrap();
        }
        for i in 3..=5 {
            insert_index_node(&conn, &IndexNodeRow {
                id: format!("topic-{i}"),
                agent_id: "a1".to_string(),
                parent_id: None, node_type: "topic".to_string(), source_type: String::new(),
                title: "T".to_string(), summary: "s".to_string(),
                start_log_id: None, end_log_id: None, source_session_id: None,
                date_from: None, date_to: None,
                depth: 0, child_count: 0, token_count: 0,
                created_at: format!("2026-01-01T00:0{i}:00Z"),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
                short_id: None,
            }).unwrap();
        }
        let count = backfill_short_ids(&conn).unwrap();
        assert_eq!(count, 3);
        // t1, t2 unchanged
        let node = get_index_node(&conn, "topic-1").unwrap().unwrap();
        assert_eq!(node.short_id, Some("t1".to_string()));
        // New ones got t3, t4, t5
        let node = get_index_node(&conn, "topic-3").unwrap().unwrap();
        assert_eq!(node.short_id, Some("t3".to_string()));
        let node = get_index_node(&conn, "topic-5").unwrap().unwrap();
        assert_eq!(node.short_id, Some("t5".to_string()));
    }

    #[test]
    fn test_backfill_short_ids_empty_table() {
        // T-1.9: No nodes -> 0 changes, no error
        let conn = setup();
        let count = backfill_short_ids(&conn).unwrap();
        assert_eq!(count, 0);
    }

    // ============================================
    // T-1.10 ~ T-1.12: date_from/date_to backfill tests
    // TODO: These tests require session_log data infrastructure setup.
    //       Implement when session_log-based date inference is added.
    // ============================================

    // ============================================
    // get_index_node_by_short_or_id tests (T-1.13 ~ T-1.15)
    // ============================================

    #[test]
    fn test_get_index_node_by_short_id() {
        // T-1.13: Search by short_id "t42"
        let conn = setup();
        insert_index_node(&conn, &IndexNodeRow {
            id: "topic-agent:nostarou:main-sess_abc-1-20".to_string(),
            agent_id: "a1".to_string(),
            parent_id: None, node_type: "topic".to_string(), source_type: String::new(),
            title: "Test Topic".to_string(), summary: "test summary".to_string(),
            start_log_id: None, end_log_id: None, source_session_id: None,
            date_from: None, date_to: None,
            depth: 0, child_count: 0, token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t42".to_string()),
        }).unwrap();
        let result = get_index_node_by_short_or_id(&conn, "a1", "t42").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().id, "topic-agent:nostarou:main-sess_abc-1-20");
    }

    #[test]
    fn test_get_index_node_by_full_id() {
        // T-1.14: Search by full id
        let conn = setup();
        insert_index_node(&conn, &IndexNodeRow {
            id: "topic-agent:nostarou:main-sess_abc-1-20".to_string(),
            agent_id: "a1".to_string(),
            parent_id: None, node_type: "topic".to_string(), source_type: String::new(),
            title: "Test Topic".to_string(), summary: "test summary".to_string(),
            start_log_id: None, end_log_id: None, source_session_id: None,
            date_from: None, date_to: None,
            depth: 0, child_count: 0, token_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            short_id: Some("t42".to_string()),
        }).unwrap();
        let result = get_index_node_by_short_or_id(&conn, "a1", "topic-agent:nostarou:main-sess_abc-1-20").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().id, "topic-agent:nostarou:main-sess_abc-1-20");
    }

    #[test]
    fn test_get_index_node_by_short_id_not_found() {
        // T-1.15: Non-existent short_id returns None
        let conn = setup();
        let result = get_index_node_by_short_or_id(&conn, "a1", "t99999").unwrap();
        assert!(result.is_none());
    }
}

// ============================================
// TRUSTED CO-AGENTS
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedCoAgentRow {
    pub id: String,
    pub agent_id: String,
    pub co_agent_id: String,
    pub allowed_actions: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

pub fn list_trusted_co_agents(conn: &Connection, agent_id: &str) -> Result<Vec<TrustedCoAgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, co_agent_id, allowed_actions, created_by, created_at
         FROM trusted_co_agents WHERE agent_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| {
        Ok(TrustedCoAgentRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            co_agent_id: row.get(2)?,
            allowed_actions: row.get(3)?,
            created_by: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn insert_trusted_co_agent(conn: &Connection, row: &TrustedCoAgentRow) -> Result<()> {
    conn.execute(
        "INSERT INTO trusted_co_agents (id, agent_id, co_agent_id, allowed_actions, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent_id, co_agent_id) DO UPDATE SET
            allowed_actions = excluded.allowed_actions,
            created_by = excluded.created_by",
        params![
            row.id,
            row.agent_id,
            row.co_agent_id,
            row.allowed_actions,
            row.created_by,
            row.created_at,
        ],
    )?;
    Ok(())
}

pub fn update_trusted_co_agent_actions(
    conn: &Connection,
    agent_id: &str,
    co_agent_id: &str,
    allowed_actions: Option<&str>,
) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE trusted_co_agents SET allowed_actions = ?3 WHERE agent_id = ?1 AND co_agent_id = ?2",
        params![agent_id, co_agent_id, allowed_actions],
    )?;
    Ok(updated > 0)
}

pub fn delete_trusted_co_agent(
    conn: &Connection,
    agent_id: &str,
    co_agent_id: &str,
) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM trusted_co_agents WHERE agent_id = ?1 AND co_agent_id = ?2",
        params![agent_id, co_agent_id],
    )?;
    Ok(deleted > 0)
}

// ============================================
// TrustedDiscordUser
// ============================================

#[derive(Debug, Clone)]
pub struct TrustedDiscordUserRow {
    pub id: String,
    pub discord_user_id: String,
    pub agent_id: String,
    pub permission: String,
    pub created_by: String,
    pub created_at: String,
}

pub fn get_trusted_user(
    conn: &Connection,
    discord_user_id: &str,
    agent_id: &str,
) -> Option<TrustedDiscordUserRow> {
    conn.query_row(
        "SELECT id, discord_user_id, agent_id, permission, created_by, created_at \
         FROM trusted_discord_users WHERE discord_user_id = ?1 AND agent_id = ?2",
        [discord_user_id, agent_id],
        |row| {
            Ok(TrustedDiscordUserRow {
                id: row.get(0)?,
                discord_user_id: row.get(1)?,
                agent_id: row.get(2)?,
                permission: row.get(3)?,
                created_by: row.get(4)?,
                created_at: row.get(5)?,
            })
        },
    )
    .ok()
}

pub fn list_trusted_users(conn: &Connection, agent_id: &str) -> Result<Vec<TrustedDiscordUserRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, discord_user_id, agent_id, permission, created_by, created_at \
         FROM trusted_discord_users WHERE agent_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([agent_id], |row| {
        Ok(TrustedDiscordUserRow {
            id: row.get(0)?,
            discord_user_id: row.get(1)?,
            agent_id: row.get(2)?,
            permission: row.get(3)?,
            created_by: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn add_trusted_user(
    conn: &Connection,
    id: &str,
    agent_id: &str,
    discord_user_id: &str,
    permission: &str,
    created_by: &str,
    created_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO trusted_discord_users (id, discord_user_id, agent_id, permission, created_by, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        [id, discord_user_id, agent_id, permission, created_by, created_at],
    )?;
    Ok(())
}

pub fn update_trusted_user_permission(
    conn: &Connection,
    id: &str,
    permission: &str,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE trusted_discord_users SET permission = ?2 WHERE id = ?1",
        [id, permission],
    )?;
    Ok(n > 0)
}

pub fn remove_trusted_user(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM trusted_discord_users WHERE id = ?1", [id])?;
    Ok(n > 0)
}

pub fn is_trusted_user(conn: &Connection, discord_user_id: &str, agent_id: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM trusted_discord_users WHERE discord_user_id = ?1 AND agent_id = ?2",
        [discord_user_id, agent_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|c| c > 0)
    .unwrap_or(false)
}

pub fn trusted_user_count(conn: &Connection, agent_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM trusted_discord_users WHERE agent_id = ?1",
        [agent_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

// ============================================
// エージェント別メモリインデックス設定
// ============================================

/// 定数: 最小値ガード
pub const BATCH_SIZE_MIN: i64 = 10;
pub const THRESHOLD_MIN: i64 = 5;
pub const BATCH_SIZE_DEFAULT: i64 = 50;
pub const THRESHOLD_DEFAULT: i64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMemoryIndexConfig {
    pub agent_id: String,
    pub batch_size: i64,
    pub threshold: i64,
    pub updated_at: String,
}

/// エージェントのメモリインデックス設定を取得（なければデフォルト値を返す）
pub fn get_memory_index_config(
    conn: &Connection,
    agent_id: &str,
) -> Result<AgentMemoryIndexConfig> {
    let result = conn.query_row(
        "SELECT agent_id, batch_size, threshold, updated_at FROM agent_memory_index_config WHERE agent_id = ?1",
        rusqlite::params![agent_id],
        |row| {
            Ok(AgentMemoryIndexConfig {
                agent_id: row.get(0)?,
                batch_size: row.get(1)?,
                threshold: row.get(2)?,
                updated_at: row.get(3)?,
            })
        },
    );

    match result {
        Ok(config) => Ok(config),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(AgentMemoryIndexConfig {
            agent_id: agent_id.to_string(),
            batch_size: BATCH_SIZE_DEFAULT,
            threshold: THRESHOLD_DEFAULT,
            updated_at: chrono::Utc::now().to_rfc3339(),
        }),
        Err(e) => Err(e.into()),
    }
}

/// エージェントのメモリインデックス設定を更新（最小値ガード付き）
pub fn upsert_memory_index_config(
    conn: &Connection,
    agent_id: &str,
    batch_size: i64,
    threshold: i64,
) -> Result<AgentMemoryIndexConfig> {
    let batch_size = batch_size.max(BATCH_SIZE_MIN);
    let threshold = threshold.max(THRESHOLD_MIN);
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO agent_memory_index_config (agent_id, batch_size, threshold, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent_id) DO UPDATE SET
             batch_size = excluded.batch_size,
             threshold = excluded.threshold,
             updated_at = excluded.updated_at",
        rusqlite::params![agent_id, batch_size, threshold, now],
    )?;

    Ok(AgentMemoryIndexConfig {
        agent_id: agent_id.to_string(),
        batch_size,
        threshold,
        updated_at: now,
    })
}

// ============================================
// AGENT ALLOWED COMMANDS
// ============================================

pub fn list_agent_allowed_commands(conn: &Connection, agent_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT command FROM agent_allowed_commands WHERE agent_id = ?1 ORDER BY added_at ASC",
    )?;
    let rows = stmt.query_map(params![agent_id], |row| row.get(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

pub fn add_agent_allowed_command(
    conn: &Connection,
    agent_id: &str,
    command: &str,
    added_by: &str,
) -> Result<bool> {
    let id = format!("{}-{}", agent_id, command);
    let now = Utc::now().to_rfc3339();
    let rows_affected = conn.execute(
        "INSERT OR IGNORE INTO agent_allowed_commands (id, agent_id, command, added_by, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, agent_id, command, added_by, now],
    )?;
    Ok(rows_affected > 0)
}

pub fn remove_agent_allowed_command(
    conn: &Connection,
    agent_id: &str,
    command: &str,
) -> Result<bool> {
    let rows_affected = conn.execute(
        "DELETE FROM agent_allowed_commands WHERE agent_id = ?1 AND command = ?2",
        params![agent_id, command],
    )?;
    Ok(rows_affected > 0)
}

// ============================================
// LLM Logs
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmLogRow {
    pub id: String,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub prompt: String,
    pub response: String,
    pub tool_calls: Option<String>,
    pub latency_ms: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub error_code: Option<String>,
    pub error_body: Option<String>,
    pub requested_at: Option<String>,
    pub trigger_message_id: Option<String>,
    pub is_bot_iteration: bool,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub created_at: String,
}

pub fn insert_llm_log(conn: &Connection, row: &LlmLogRow) -> Result<()> {
    conn.execute(
        "INSERT INTO llm_logs (id, agent_id, session_id, model, prompt, response, tool_calls, latency_ms, prompt_tokens, completion_tokens, total_tokens, error_code, error_body, requested_at, trigger_message_id, is_bot_iteration, cache_read_tokens, cache_creation_tokens, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            row.id,
            row.agent_id,
            row.session_id,
            row.model,
            row.prompt,
            row.response,
            row.tool_calls,
            row.latency_ms,
            row.prompt_tokens,
            row.completion_tokens,
            row.total_tokens,
            row.error_code,
            row.error_body,
            row.requested_at,
            row.trigger_message_id,
            row.is_bot_iteration,
            row.cache_read_tokens,
            row.cache_creation_tokens,
            row.created_at,
        ],
    )?;
    Ok(())
}

pub fn list_llm_logs(conn: &Connection, agent_id: &str, limit: i64) -> Result<Vec<LlmLogRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, session_id, model, prompt, response, tool_calls,
                latency_ms, prompt_tokens, completion_tokens, total_tokens,
                error_code, error_body, requested_at, trigger_message_id,
                is_bot_iteration, cache_read_tokens, cache_creation_tokens, created_at
         FROM llm_logs
         WHERE agent_id = ?1
         ORDER BY created_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![agent_id, limit], |row| {
        Ok(LlmLogRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            session_id: row.get(2)?,
            model: row.get(3)?,
            prompt: row.get(4)?,
            response: row.get(5)?,
            tool_calls: row.get(6)?,
            latency_ms: row.get(7)?,
            prompt_tokens: row.get(8)?,
            completion_tokens: row.get(9)?,
            total_tokens: row.get(10)?,
            error_code: row.get(11)?,
            error_body: row.get(12)?,
            requested_at: row.get(13)?,
            trigger_message_id: row.get(14)?,
            is_bot_iteration: row.get::<_, i64>(15).map(|v| v != 0).unwrap_or(false),
            cache_read_tokens: row.get(16)?,
            cache_creation_tokens: row.get(17)?,
            created_at: row.get(18)?,
        })
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmLogStatRow {
    pub date: String,
    pub count: i64,
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub avg_latency_ms: f64,
    pub error_count: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
}

pub fn llm_logs_stats(conn: &Connection, agent_id: &str, days: i64) -> Result<Vec<LlmLogStatRow>> {
    let sql = "SELECT date(COALESCE(requested_at, created_at)) as date,
               COUNT(*) as count,
               COALESCE(SUM(total_tokens),0) as total_tokens,
               COALESCE(SUM(prompt_tokens),0) as prompt_tokens,
               COALESCE(SUM(completion_tokens),0) as completion_tokens,
               COALESCE(AVG(latency_ms),0) as avg_latency_ms,
               COUNT(CASE WHEN error_code IS NOT NULL THEN 1 END) as error_count,
               COALESCE(SUM(cache_read_tokens),0) as cache_read_tokens,
               COALESCE(SUM(cache_creation_tokens),0) as cache_creation_tokens
        FROM llm_logs
        WHERE agent_id = ?1
          AND COALESCE(requested_at, created_at) >= datetime('now', ?2)
        GROUP BY date(COALESCE(requested_at, created_at))
        ORDER BY date ASC";
    let days_param = format!("-{} days", days);
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![agent_id, days_param], |row| {
        Ok(LlmLogStatRow {
            date: row.get(0)?,
            count: row.get(1)?,
            total_tokens: row.get(2)?,
            prompt_tokens: row.get(3)?,
            completion_tokens: row.get(4)?,
            avg_latency_ms: row.get(5)?,
            error_count: row.get(6)?,
            cache_read_tokens: row.get(7)?,
            cache_creation_tokens: row.get(8)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.into())
}

// ============================================
// IMPORT SYNC STATE
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStateRow {
    pub id: String,
    pub agent_id: String,
    pub source_dir: String,
    pub file_type: String,
    pub file_name: String,
    pub content_hash: String,
    pub synced_at: String,
    pub created_at: String,
}

pub fn get_sync_state(
    conn: &Connection,
    agent_id: &str,
    source_dir: &str,
    file_name: &str,
) -> Result<Option<SyncStateRow>> {
    let result = conn.query_row(
        "SELECT id, agent_id, source_dir, file_type, file_name, content_hash, synced_at, created_at
         FROM import_sync_state
         WHERE agent_id = ?1 AND source_dir = ?2 AND file_name = ?3",
        params![agent_id, source_dir, file_name],
        |row| {
            Ok(SyncStateRow {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                source_dir: row.get(2)?,
                file_type: row.get(3)?,
                file_name: row.get(4)?,
                content_hash: row.get(5)?,
                synced_at: row.get(6)?,
                created_at: row.get(7)?,
            })
        },
    );
    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn upsert_sync_state(conn: &Connection, row: &SyncStateRow) -> Result<()> {
    conn.execute(
        "INSERT INTO import_sync_state (id, agent_id, source_dir, file_type, file_name, content_hash, synced_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(agent_id, source_dir, file_name) DO UPDATE SET
            content_hash = excluded.content_hash,
            synced_at = excluded.synced_at,
            file_type = excluded.file_type",
        params![
            row.id,
            row.agent_id,
            row.source_dir,
            row.file_type,
            row.file_name,
            row.content_hash,
            row.synced_at,
            row.created_at,
        ],
    )?;
    Ok(())
}

pub fn list_sync_states(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<SyncStateRow>, i64)> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM import_sync_state WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, agent_id, source_dir, file_type, file_name, content_hash, synced_at, created_at
         FROM import_sync_state WHERE agent_id = ?1
         ORDER BY synced_at DESC LIMIT ?2 OFFSET ?3",
    )?;

    let rows = stmt.query_map(params![agent_id, limit, offset], |row| {
        Ok(SyncStateRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            source_dir: row.get(2)?,
            file_type: row.get(3)?,
            file_name: row.get(4)?,
            content_hash: row.get(5)?,
            synced_at: row.get(6)?,
            created_at: row.get(7)?,
        })
    })?;

    Ok((rows.collect::<std::result::Result<_, _>>()?, total))
}

pub fn delete_sync_states_for_agent(conn: &Connection, agent_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM import_sync_state WHERE agent_id = ?1",
        params![agent_id],
    )?;
    Ok(())
}

pub fn get_latest_sync_at(conn: &Connection, agent_id: &str) -> Result<Option<String>> {
    let result = conn.query_row(
        "SELECT MAX(synced_at) FROM import_sync_state WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get::<_, Option<String>>(0),
    );
    match result {
        Ok(val) => Ok(val),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

// ============================================================
// agent_logs
// ============================================================

#[derive(Debug, Clone)]
pub struct AgentLogRow {
    pub id: String,
    pub agent_id: Option<String>,
    pub level: String,
    pub context: String,
    pub message: String,
    pub created_at: Option<String>,
}

pub fn insert_agent_log(conn: &Connection, row: &AgentLogRow) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_logs (id, agent_id, level, context, message, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, COALESCE(?6, datetime('now')))",
        rusqlite::params![
            row.id,
            row.agent_id,
            row.level,
            row.context,
            row.message,
            row.created_at,
        ],
    )?;
    Ok(())
}

pub fn list_agent_logs(
    conn: &Connection,
    agent_id: Option<&str>,
    level_filter: Option<&str>,
    limit: i64,
) -> Result<Vec<AgentLogRow>> {
    let mut rows = Vec::new();
    match (agent_id, level_filter) {
        (Some(aid), Some(lvl)) => {
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, level, context, message, created_at FROM agent_logs WHERE agent_id=?1 AND level=?2 ORDER BY created_at DESC LIMIT ?3",
            )?;
            for r in stmt.query_map(rusqlite::params![aid, lvl, limit], |row| {
                Ok(AgentLogRow {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    level: row.get(2)?,
                    context: row.get(3)?,
                    message: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })? {
                rows.push(r?);
            }
        }
        (Some(aid), None) => {
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, level, context, message, created_at FROM agent_logs WHERE agent_id=?1 ORDER BY created_at DESC LIMIT ?2",
            )?;
            for r in stmt.query_map(rusqlite::params![aid, limit], |row| {
                Ok(AgentLogRow {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    level: row.get(2)?,
                    context: row.get(3)?,
                    message: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })? {
                rows.push(r?);
            }
        }
        (None, Some(lvl)) => {
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, level, context, message, created_at FROM agent_logs WHERE level=?1 ORDER BY created_at DESC LIMIT ?2",
            )?;
            for r in stmt.query_map(rusqlite::params![lvl, limit], |row| {
                Ok(AgentLogRow {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    level: row.get(2)?,
                    context: row.get(3)?,
                    message: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })? {
                rows.push(r?);
            }
        }
        (None, None) => {
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, level, context, message, created_at FROM agent_logs ORDER BY created_at DESC LIMIT ?1",
            )?;
            for r in stmt.query_map(rusqlite::params![limit], |row| {
                Ok(AgentLogRow {
                    id: row.get(0)?,
                    agent_id: row.get(1)?,
                    level: row.get(2)?,
                    context: row.get(3)?,
                    message: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })? {
                rows.push(r?);
            }
        }
    }
    Ok(rows)
}

// ============================================
// PENDING INTERACTIONS (A2UI)
// ============================================

#[derive(Debug, Clone)]
pub struct PendingInteractionRow {
    pub id: String,
    pub agent_id: String,
    pub session_id: String,
    pub channel_id: String,
    pub message_id: Option<String>,
    pub platform: String,
    pub surface_id: String,
    pub a2ui_components_json: String,
    pub status: String,
    pub response_json: Option<String>,
    pub responder_id: Option<String>,
    pub owner_only: bool,
    pub timeout_secs: i64,
    pub created_at: String,
    pub responded_at: Option<String>,
    pub updated_at: String,
}

/// pending_interactions テーブルへの挿入
pub fn insert_pending_interaction(
    conn: &Connection,
    id: &str,
    agent_id: &str,
    session_id: &str,
    channel_id: &str,
    message_id: Option<&str>,
    platform: &str,
    surface_id: &str,
    a2ui_components_json: &str,
    owner_only: bool,
    timeout_secs: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pending_interactions (id, agent_id, session_id, channel_id, message_id, platform, surface_id, a2ui_components_json, owner_only, timeout_secs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![id, agent_id, session_id, channel_id, message_id, platform, surface_id, a2ui_components_json, owner_only as i32, timeout_secs],
    )?;
    Ok(())
}

/// pending_interaction の取得
pub fn get_pending_interaction(
    conn: &Connection,
    id: &str,
) -> Result<Option<PendingInteractionRow>> {
    let result = conn.query_row(
        "SELECT id, agent_id, session_id, channel_id, message_id, platform, surface_id, a2ui_components_json, status, response_json, responder_id, owner_only, timeout_secs, created_at, responded_at, updated_at
         FROM pending_interactions WHERE id = ?1",
        params![id],
        |row| {
            Ok(PendingInteractionRow {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                session_id: row.get(2)?,
                channel_id: row.get(3)?,
                message_id: row.get(4)?,
                platform: row.get(5)?,
                surface_id: row.get(6)?,
                a2ui_components_json: row.get(7)?,
                status: row.get(8)?,
                response_json: row.get(9)?,
                responder_id: row.get(10)?,
                owner_only: row.get::<_, i32>(11)? != 0,
                timeout_secs: row.get(12)?,
                created_at: row.get(13)?,
                responded_at: row.get(14)?,
                updated_at: row.get(15)?,
            })
        },
    );
    match result {
        Ok(r) => Ok(Some(r)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// pending_interaction のステータス更新
pub fn update_pending_interaction_status(
    conn: &Connection,
    id: &str,
    status: &str,
    response_json: Option<&str>,
    responder_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE pending_interactions SET status = ?2, response_json = ?3, responder_id = ?4, responded_at = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
        params![id, status, response_json, responder_id],
    )?;
    Ok(())
}

pub fn next_short_id(conn: &Connection, agent_id: &str, prefix: &str) -> Result<String> {
    let max: Option<i64> = conn
        .query_row(
            "SELECT MAX(CAST(SUBSTR(short_id, ?3) AS INTEGER)) FROM memory_index_nodes WHERE agent_id = ?1 AND short_id LIKE ?2",
            params![agent_id, format!("{prefix}%"), (prefix.len() + 1) as i64],
            |row| row.get(0),
        )
        .unwrap_or(None);
    Ok(format!("{prefix}{}", max.unwrap_or(0) + 1))
}

pub fn backfill_short_ids(conn: &Connection) -> Result<usize> {
    let agent_ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT DISTINCT agent_id FROM memory_index_nodes WHERE short_id IS NULL")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let mut total = 0usize;
    for agent_id in &agent_ids {
        let nodes: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, node_type FROM memory_index_nodes WHERE agent_id = ?1 AND short_id IS NULL ORDER BY created_at ASC"
            )?;
            let rows = stmt.query_map(params![agent_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for (node_id, node_type) in &nodes {
            let prefix = match node_type.as_str() {
                "topic" => "t",
                "period" => "p",
                "daily" => "d",
                "session" => "s",
                "hourly" => "h",
                "weekly" => "w",
                "monthly" => "m",
                "yearly" => "y",
                "root" => "r",
                _ => "x",
            };
            let sid = next_short_id(conn, agent_id, prefix)?;
            conn.execute(
                "UPDATE memory_index_nodes SET short_id = ?1 WHERE id = ?2",
                params![sid, node_id],
            )?;
            total += 1;
        }
    }
    Ok(total)
}

pub fn get_index_node_by_short_or_id(conn: &Connection, agent_id: &str, query: &str) -> Result<Option<IndexNodeRow>> {
    let result = conn.query_row(
        "SELECT id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id, end_log_id, source_session_id, date_from, date_to, depth, child_count, token_count, created_at, updated_at, short_id
         FROM memory_index_nodes WHERE agent_id = ?1 AND short_id = ?2",
        params![agent_id, query],
        |row| {
            Ok(IndexNodeRow {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                parent_id: row.get(2)?,
                node_type: row.get(3)?,
                source_type: row.get(4)?,
                title: row.get(5)?,
                summary: row.get(6)?,
                start_log_id: row.get(7)?,
                end_log_id: row.get(8)?,
                source_session_id: row.get(9)?,
                date_from: row.get(10)?,
                date_to: row.get(11)?,
                depth: row.get(12)?,
                child_count: row.get(13)?,
                token_count: row.get(14)?,
                created_at: row.get(15)?,
                updated_at: row.get(16)?,
                short_id: row.get(17)?,
            })
        },
    );
    match result {
        Ok(node) => Ok(Some(node)),
        Err(rusqlite::Error::QueryReturnedNoRows) => get_index_node(conn, query),
        Err(e) => Err(e.into()),
    }
}

/// stale pending interactions のクリーンアップ（起動時に呼ぶ）
pub fn cleanup_stale_pending_interactions(conn: &Connection) -> Result<usize> {
    let count = conn.execute(
        "UPDATE pending_interactions SET status = 'timeout', updated_at = datetime('now') WHERE status = 'pending'",
        [],
    )?;
    Ok(count)
}
