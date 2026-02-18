use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

// ============================================
// Shared DTOs (available on both client & server)
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentSummary {
    pub id: String,
    pub name: String,
    pub persona_name: String,
    pub role: String,
    pub image_url: Option<String>,
    pub status: String,
    pub skill_count: i32,
    pub session_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentDetail {
    pub id: String,
    pub name: String,
    pub role: String,
    pub job_title: Option<String>,
    pub organization: Option<String>,
    pub image_url: Option<String>,
    pub persona_name: String,
    pub social_style_json: String,
    pub personality_json: String,
    pub thinking_style_json: String,
    pub custom_traits_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersonalityDto {
    pub openness: f32,
    pub conscientiousness: f32,
    pub extraversion: f32,
    pub agreeableness: f32,
    pub neuroticism: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SoulDto {
    pub persona_name: String,
    pub social_style_json: String,
    pub personality: PersonalityDto,
    pub thinking_style_primary: String,
    pub thinking_style_secondary: String,
    pub thinking_style_description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillDto {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source_type: String,
    pub effectiveness: Option<f64>,
    pub usage_count: i32,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CuratedMemoryDto {
    pub id: String,
    pub category: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionLogDto {
    pub id: i64,
    pub session_id: String,
    pub log_type: String,
    pub content: String,
    pub speaker_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionDto {
    pub id: String,
    pub mode: String,
    pub theme: String,
    pub phase: String,
    pub turn_number: i32,
    pub status: String,
    pub participant_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceEntryDto {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmMetricsSummaryDto {
    pub count: i64,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub avg_latency: f64,
    pub avg_quality: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmMetricsDetailDto {
    pub provider: String,
    pub model: String,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub request_count: i64,
    pub avg_latency: f64,
}

// ============================================
// Server Functions
// ============================================

#[cfg(feature = "server")]
fn db_path() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let project_root = std::path::Path::new(manifest_dir).parent().unwrap();
    project_root.join("data/opencrab.db").to_string_lossy().into_owned()
}


#[server]
pub async fn get_agents() -> Result<Vec<AgentSummary>, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let mut stmt = conn
        .prepare(
            "SELECT i.agent_id, i.name, COALESCE(s.persona_name, ''), i.role, i.image_url,
                    (SELECT COUNT(*) FROM skills WHERE agent_id = i.agent_id) as skill_count,
                    (SELECT COUNT(*) FROM agent_sessions WHERE agent_id = i.agent_id) as session_count
             FROM identity i
             LEFT JOIN soul s ON i.agent_id = s.agent_id",
        )
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let rows = stmt
        .query_map([], |row| {
            Ok(AgentSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                persona_name: row.get(2)?,
                role: row.get(3)?,
                image_url: row.get(4)?,
                status: "idle".to_string(),
                skill_count: row.get(5)?,
                session_count: row.get(6)?,
            })
        })
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let agents: Vec<AgentSummary> = rows.filter_map(|r| r.ok()).collect();
    Ok(agents)
}

#[server]
pub async fn get_agent(id: String) -> Result<AgentDetail, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let identity = opencrab_db::queries::get_identity(&conn, &id)
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .ok_or_else(|| ServerFnError::new("Agent not found"))?;

    let soul = opencrab_db::queries::get_soul(&conn, &id)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(AgentDetail {
        id: identity.agent_id,
        name: identity.name,
        role: identity.role,
        job_title: identity.job_title,
        organization: identity.organization,
        image_url: identity.image_url,
        persona_name: soul.as_ref().map(|s| s.persona_name.clone()).unwrap_or_default(),
        social_style_json: soul.as_ref().map(|s| s.social_style_json.clone()).unwrap_or_else(|| "{}".to_string()),
        personality_json: soul.as_ref().map(|s| s.personality_json.clone()).unwrap_or_else(|| "{}".to_string()),
        thinking_style_json: soul.as_ref().map(|s| s.thinking_style_json.clone()).unwrap_or_else(|| "{}".to_string()),
        custom_traits_json: soul.and_then(|s| s.custom_traits_json),
    })
}

#[server]
pub async fn update_soul(agent_id: String, soul: SoulDto) -> Result<(), ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let personality_json = serde_json::to_string(&soul.personality)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let thinking_style = serde_json::json!({
        "primary": soul.thinking_style_primary,
        "secondary": soul.thinking_style_secondary,
        "description": soul.thinking_style_description,
    });

    let soul_row = opencrab_db::queries::SoulRow {
        agent_id,
        persona_name: soul.persona_name,
        social_style_json: soul.social_style_json,
        personality_json,
        thinking_style_json: thinking_style.to_string(),
        custom_traits_json: None,
    };

    opencrab_db::queries::upsert_soul(&conn, &soul_row)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(())
}

#[server]
pub async fn create_agent(
    name: String,
    role: String,
    persona_name: String,
) -> Result<AgentSummary, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let agent_id = uuid::Uuid::new_v4().to_string();

    let identity = opencrab_db::queries::IdentityRow {
        agent_id: agent_id.clone(),
        name: name.clone(),
        role: if role.is_empty() { "discussant".to_string() } else { role.clone() },
        job_title: None,
        organization: None,
        image_url: None,
        metadata_json: None,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let pname = if persona_name.is_empty() { name.clone() } else { persona_name };
    let soul = opencrab_db::queries::SoulRow {
        agent_id: agent_id.clone(),
        persona_name: pname.clone(),
        social_style_json: "{}".to_string(),
        personality_json: r#"{"openness":0.5,"conscientiousness":0.5,"extraversion":0.5,"agreeableness":0.5,"neuroticism":0.0}"#.to_string(),
        thinking_style_json: r#"{"primary":"Analytical","secondary":"Practical","description":""}"#.to_string(),
        custom_traits_json: None,
    };
    opencrab_db::queries::upsert_soul(&conn, &soul)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(AgentSummary {
        id: agent_id,
        name,
        persona_name: pname,
        role: identity.role,
        image_url: None,
        status: "idle".to_string(),
        skill_count: 0,
        session_count: 0,
    })
}

#[server]
pub async fn update_identity(
    agent_id: String,
    name: String,
    role: String,
    job_title: Option<String>,
    organization: Option<String>,
) -> Result<(), ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let existing = opencrab_db::queries::get_identity(&conn, &agent_id)
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .ok_or_else(|| ServerFnError::new("Agent not found"))?;

    let identity = opencrab_db::queries::IdentityRow {
        agent_id,
        name,
        role,
        job_title,
        organization,
        image_url: existing.image_url,
        metadata_json: existing.metadata_json,
    };
    opencrab_db::queries::upsert_identity(&conn, &identity)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(())
}

#[server]
pub async fn delete_agent(agent_id: String) -> Result<bool, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let deleted = opencrab_db::queries::delete_agent(&conn, &agent_id)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(deleted)
}

#[server]
pub async fn get_skills(agent_id: String) -> Result<Vec<SkillDto>, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let skills = opencrab_db::queries::list_skills(&conn, &agent_id, false)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(skills
        .into_iter()
        .map(|s| SkillDto {
            id: s.id,
            name: s.name,
            description: s.description,
            source_type: s.source_type,
            effectiveness: s.effectiveness,
            usage_count: s.usage_count,
            is_active: s.is_active,
        })
        .collect())
}

#[server]
pub async fn toggle_skill(skill_id: String, active: bool) -> Result<(), ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    opencrab_db::queries::set_skill_active(&conn, &skill_id, active)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(())
}

#[server]
pub async fn get_curated_memories(agent_id: String) -> Result<Vec<CuratedMemoryDto>, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let memories = opencrab_db::queries::list_curated_memories(&conn, &agent_id)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(memories
        .into_iter()
        .map(|m| CuratedMemoryDto {
            id: m.id,
            category: m.category,
            content: m.content,
        })
        .collect())
}

#[server]
pub async fn search_session_logs(
    agent_id: String,
    query: String,
) -> Result<Vec<SessionLogDto>, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let results = opencrab_db::queries::search_session_logs(&conn, &agent_id, &query, 50)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(results
        .into_iter()
        .map(|r| SessionLogDto {
            id: r.id,
            session_id: r.session_id,
            log_type: r.log_type,
            content: r.content,
            speaker_id: None,
            created_at: r.created_at,
        })
        .collect())
}

#[server]
pub async fn get_sessions() -> Result<Vec<SessionDto>, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let sessions = opencrab_db::queries::list_sessions(&conn)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(sessions
        .into_iter()
        .map(|s| {
            let participant_count: usize = serde_json::from_str::<Vec<String>>(&s.participant_ids_json)
                .map(|v| v.len())
                .unwrap_or(0);
            SessionDto {
                id: s.id,
                mode: s.mode,
                theme: s.theme,
                phase: s.phase,
                turn_number: s.turn_number,
                status: s.status,
                participant_count,
            }
        })
        .collect())
}

#[server]
pub async fn get_session(id: String) -> Result<SessionDto, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let s = opencrab_db::queries::get_session(&conn, &id)
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .ok_or_else(|| ServerFnError::new("Session not found"))?;

    let participant_count: usize = serde_json::from_str::<Vec<String>>(&s.participant_ids_json)
        .map(|v| v.len())
        .unwrap_or(0);

    Ok(SessionDto {
        id: s.id,
        mode: s.mode,
        theme: s.theme,
        phase: s.phase,
        turn_number: s.turn_number,
        status: s.status,
        participant_count,
    })
}

#[server]
pub async fn get_session_logs(session_id: String) -> Result<Vec<SessionLogDto>, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, log_type, content, speaker_id, created_at
             FROM memory_sessions
             WHERE session_id = ?1
             ORDER BY created_at ASC",
        )
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let rows = stmt
        .query_map(rusqlite::params![session_id], |row| {
            Ok(SessionLogDto {
                id: row.get(0)?,
                session_id: row.get(1)?,
                log_type: row.get(2)?,
                content: row.get(3)?,
                speaker_id: row.get(4)?,
                created_at: row.get(5)?,
            })
        })
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(rows.filter_map(|r| r.ok()).collect())
}

#[server]
pub async fn send_mentor_instruction(
    session_id: String,
    content: String,
) -> Result<(), ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: "mentor".to_string(),
        session_id,
        log_type: "system".to_string(),
        content,
        speaker_id: Some("mentor".to_string()),
        turn_number: None,
        metadata_json: None,
    };
    opencrab_db::queries::insert_session_log(&conn, &log)
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(())
}

#[server]
pub async fn get_llm_metrics(
    agent_id: String,
    period: String,
) -> Result<LlmMetricsSummaryDto, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let since = match period.as_str() {
        "day" => chrono::Utc::now() - chrono::Duration::days(1),
        "week" => chrono::Utc::now() - chrono::Duration::weeks(1),
        "month" => chrono::Utc::now() - chrono::Duration::days(30),
        _ => chrono::Utc::now() - chrono::Duration::weeks(1),
    };

    let summary = opencrab_db::queries::get_llm_metrics_summary(&conn, &agent_id, &since.to_rfc3339())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(LlmMetricsSummaryDto {
        count: summary.count,
        total_tokens: summary.total_tokens.unwrap_or(0),
        total_cost: summary.total_cost.unwrap_or(0.0),
        avg_latency: summary.avg_latency.unwrap_or(0.0),
        avg_quality: summary.avg_quality.unwrap_or(0.0),
    })
}

#[server]
pub async fn get_llm_metrics_detail(
    agent_id: String,
    period: String,
) -> Result<Vec<LlmMetricsDetailDto>, ServerFnError> {
    let conn = opencrab_db::init_connection(&db_path())
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let since = match period.as_str() {
        "day" => chrono::Utc::now() - chrono::Duration::days(1),
        "week" => chrono::Utc::now() - chrono::Duration::weeks(1),
        "month" => chrono::Utc::now() - chrono::Duration::days(30),
        _ => chrono::Utc::now() - chrono::Duration::weeks(1),
    };

    let mut stmt = conn
        .prepare(
            "SELECT provider, model, SUM(total_tokens), SUM(estimated_cost_usd), COUNT(*), AVG(latency_ms)
             FROM llm_usage_metrics
             WHERE agent_id = ?1 AND timestamp >= ?2
             GROUP BY provider, model
             ORDER BY SUM(estimated_cost_usd) DESC",
        )
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    let rows = stmt
        .query_map(rusqlite::params![agent_id, since.to_rfc3339()], |row| {
            Ok(LlmMetricsDetailDto {
                provider: row.get(0)?,
                model: row.get(1)?,
                total_tokens: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                total_cost: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                request_count: row.get(4)?,
                avg_latency: row.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
            })
        })
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    Ok(rows.filter_map(|r| r.ok()).collect())
}
