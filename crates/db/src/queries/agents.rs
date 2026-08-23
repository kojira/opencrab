use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

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
    /// ハートビート専用の自律行動指示。空文字なら既定文言にフォールバックする。
    #[serde(default)]
    pub heartbeat_instructions: String,
    pub model: Option<String>,
    /// 推論（thinking）強度。None/空 = 既定に従う。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// 本文URL読取り（プロバイダネイティブの web_search / url_context）。
    /// None = 無効（オプトイン）。
    #[serde(default)]
    pub web_search: Option<bool>,
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
    pub heartbeat_instructions: Option<String>,
    pub model: Option<Option<String>>,
    pub reasoning_effort: Option<Option<String>>,
    pub web_search: Option<Option<bool>>,
    pub metadata_json: Option<Option<String>>,
}

pub fn upsert_agent(conn: &Connection, agent: &AgentRow) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO agents (agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, heartbeat_instructions, model, reasoning_effort, web_search, metadata_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(agent_id) DO UPDATE SET
            name = excluded.name,
            job_title = excluded.job_title,
            organization = excluded.organization,
            image_url = excluded.image_url,
            persona_name = excluded.persona_name,
            personality = excluded.personality,
            instructions = excluded.instructions,
            heartbeat_instructions = excluded.heartbeat_instructions,
            model = excluded.model,
            reasoning_effort = excluded.reasoning_effort,
            web_search = excluded.web_search,
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
            agent.heartbeat_instructions,
            agent.model,
            agent.reasoning_effort,
            agent.web_search,
            agent.metadata_json,
            now,
            now,
        ],
    )?;
    Ok(())
}

pub fn get_agent(conn: &Connection, agent_id: &str) -> Result<Option<AgentRow>> {
    let result = conn.query_row(
        "SELECT agent_id, name, job_title, organization, image_url, persona_name, personality, instructions, heartbeat_instructions, model, reasoning_effort, web_search, metadata_json
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
                heartbeat_instructions: row.get(8)?,
                model: row.get(9)?,
                reasoning_effort: row.get(10)?,
                web_search: row.get(11)?,
                metadata_json: row.get(12)?,
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

/// `agents.reasoning_effort` が空でなければそれを返す（per-agent の thinking 強度）。
/// 未設定なら None（プロバイダー/モデル既定に従う）。
pub fn effective_reasoning_effort_for_agent(
    conn: &Connection,
    agent_id: &str,
) -> Result<Option<String>> {
    Ok(get_agent(conn, agent_id)?
        .and_then(|a| a.reasoning_effort)
        .filter(|s| !s.trim().is_empty()))
}

/// `agents.web_search` が true なら本文URL読取り（provider native）を有効にする。
pub fn web_search_enabled_for_agent(conn: &Connection, agent_id: &str) -> Result<bool> {
    Ok(get_agent(conn, agent_id)?
        .and_then(|a| a.web_search)
        .unwrap_or(false))
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
    if let Some(ref v) = patch.heartbeat_instructions {
        row.heartbeat_instructions = v.clone();
    }
    if let Some(ref v) = patch.model {
        row.model = v.clone();
    }
    if let Some(ref v) = patch.reasoning_effort {
        // 空文字は「未設定」に正規化して NULL 保存（UI は既定選択時に "" を送る）。
        // serde の Option<Option<_>> は JSON null を「変更なし」に潰すため、
        // クリアは null ではなく空文字で表現する。
        row.reasoning_effort = v.clone().filter(|s| !s.trim().is_empty());
    }
    if let Some(ref v) = patch.web_search {
        row.web_search = *v;
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
    let deleted = conn.execute("DELETE FROM agents WHERE agent_id = ?1", params![agent_id])?;
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

/// 全エージェントの agent_id を返す（メンテナンスループの巡回用）。
pub fn list_agent_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT agent_id FROM agents ORDER BY agent_id")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
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
