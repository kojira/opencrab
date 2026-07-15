use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

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

// ============================================
// Skill usage log (スリープ棚卸しの弱い利用ヒント)
// ============================================

/// スキルが使われた（応答に名前が出た）ことをセッション単位で記録する。
/// 名前一致ベースなのでノイズがあり、棚卸しでは弱いヒントとしてのみ使う。
pub fn insert_skill_usage(
    conn: &Connection,
    agent_id: &str,
    skill_id: &str,
    session_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO skill_usage_log (agent_id, skill_id, session_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![agent_id, skill_id, session_id, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

/// 指定時刻以降に当該スキルが使われた distinct セッション ID を返す（弱い利用ヒント）。
/// `since` が None なら全期間。
pub fn list_skill_used_sessions(
    conn: &Connection,
    skill_id: &str,
    since: Option<&str>,
) -> Result<Vec<String>> {
    let collect =
        |stmt: &mut rusqlite::Statement, p: &[&dyn rusqlite::ToSql]| -> Result<Vec<String>> {
            let rows = stmt.query_map(p, |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        };
    match since {
        Some(ts) => {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT session_id FROM skill_usage_log
                 WHERE skill_id = ?1 AND created_at >= ?2 ORDER BY created_at DESC",
            )?;
            collect(&mut stmt, params![skill_id, ts])
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT session_id FROM skill_usage_log
                 WHERE skill_id = ?1 ORDER BY created_at DESC",
            )?;
            collect(&mut stmt, params![skill_id])
        }
    }
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
