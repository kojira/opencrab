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
    /// 作成時 caller の trust class（`"owner"` / `"trusted"` / `"agent"`）。
    /// `None` = この列より前に作られた既存スキル（legacy grandfather = Owner 相当）。
    /// `read_skill` が confused deputy を塞ぐために参照する（#335）。
    pub created_caller: Option<String>,
    /// caller=Agent のターン（素の Agent 権限で走る run。外部 Nostr の受信ターンが典型例
    /// だが判定軸は transport ではなく caller=Agent）へ index / read_skill 本文を露出して
    /// よいか。既定 false（fail-closed）。オーナーが REST で少数だけ true にする（#352）。
    pub agent_visible: bool,
    /// このスキルを開いた（`read_skill`）ターンで使えるようになるツール名（D-1143）。
    /// 効くのは `agent_visible`（オーナーが「誰でも使える」に指定）のスキルだけ。判定は
    /// [`SkillRow::granted_tools`] の 1 か所。
    #[serde(default)]
    pub tools: Vec<String>,
}

/// エージェントが自分で学んだスキルの `source_type`（D-1144）。学んだ振る舞いはどの会話でも効かせるので、
/// caller を問わず一覧に出し `read_skill` で読める（ツールは許さない）。作るもの:
/// - `"experience"`: `learn_from_experience`
/// - `"sleep_curated"`: スリープ時スキル棚卸しの create
/// - `"peer"`: `learn_from_peer`
pub const LEARNED_SKILL_SOURCE_TYPES: &[&str] = &["experience", "sleep_curated", "peer"];

impl SkillRow {
    /// このスキルを caller を問わず一覧に出し、`read_skill` で本文を渡してよいか（D-1144）。
    /// オーナーが「誰でも使える」に指定したスキルと、エージェントが学んだスキル
    /// （[`LEARNED_SKILL_SOURCE_TYPES`]）。見えることとツールを許すことは別で、ツールは
    /// [`SkillRow::granted_tools`]（指定されたスキルだけ）が決める。
    pub fn visible_to_any_caller(&self) -> bool {
        self.agent_visible || LEARNED_SKILL_SOURCE_TYPES.contains(&self.source_type.as_str())
    }

    /// このスキルを開いたターンへ、caller を問わず許すツール（D-1143）。オーナーが
    /// 「誰でも使える」に指定した（`agent_visible`）スキルの `tools` だけ。指定が無ければ空。
    pub fn granted_tools(&self) -> &[String] {
        if self.agent_visible {
            &self.tools
        } else {
            &[]
        }
    }

    /// 「誰でも使える」指定（`anyone_can_use` = `agent_visible`）とツール一覧を変える（D-1143）。
    /// 変えてよいかの規則はここだけ: 指定を立てるのと、指定済みスキルのツールを変えるのは
    /// オーナー等価の書き手だけ（会話でオーナーが頼んだときのエージェント、またはダッシュボード）。
    /// それ以外は何も変えずに理由を返す。`None` は変えない。
    pub fn set_access(
        &mut self,
        anyone_can_use: Option<bool>,
        tools: Option<Vec<String>>,
        writer_is_owner_equivalent: bool,
    ) -> std::result::Result<(), &'static str> {
        if !writer_is_owner_equivalent {
            if anyone_can_use == Some(true) && !self.agent_visible {
                return Err(OWNER_REQUIRED_FOR_DESIGNATION);
            }
            if self.agent_visible && tools.as_ref().is_some_and(|t| *t != self.tools) {
                return Err(OWNER_REQUIRED_FOR_DESIGNATION);
            }
        }
        if let Some(anyone_can_use) = anyone_can_use {
            self.agent_visible = anyone_can_use;
        }
        if let Some(tools) = tools {
            self.tools = tools;
        }
        Ok(())
    }

    /// 既存スキルの本文やツールを書き換えたときに呼ぶ（D-1143）。オーナー等価でない書き手が
    /// 書き換えたら、それはもうオーナーが指定した中身ではないので「誰でも使える」指定
    /// （`agent_visible`）を外す（オーナーが指定し直す）。外したら `true` を返す。
    pub fn record_rewrite(&mut self, writer_is_owner_equivalent: bool) -> bool {
        if writer_is_owner_equivalent || !self.agent_visible {
            return false;
        }
        self.agent_visible = false;
        true
    }
}

/// [`SkillRow::set_access`] が非オーナーの指定変更を断るときの文。
pub const OWNER_REQUIRED_FOR_DESIGNATION: &str = "Only the owner can make a skill usable by anyone or change the tools of such a skill. Do it in a turn where the owner asks for it (or the owner uses the dashboard).";

/// [`SkillRow::record_rewrite`] が指定を外したとき、書き換えたツールの結果に添える文。
pub const OWNER_DESIGNATION_CLEARED_NOTE: &str = "This skill was designated by the owner as usable by anyone. Because it was rewritten from a non-owner turn, that designation was removed; the owner must designate it again.";

/// `tools` 列（JSON 配列の文字列）を読む。壊れた値は空として扱う。
fn tools_from_column(raw: String) -> Vec<String> {
    serde_json::from_str(&raw).unwrap_or_default()
}

fn tools_to_column(tools: &[String]) -> String {
    serde_json::to_string(tools).unwrap_or_else(|_| "[]".to_string())
}

/// 全 skills SELECT が共有する列並び。順序は [`skill_row_from_row`] の `row.get` と一致させる。
const SKILL_COLUMNS: &str = "id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, permission, archived, created_caller, agent_visible, tools";

/// [`SKILL_COLUMNS`] の並びで 1 行を [`SkillRow`] へ写す。全 SELECT の重複を 1 箇所に集約する。
fn skill_row_from_row(row: &rusqlite::Row) -> rusqlite::Result<SkillRow> {
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
        created_caller: row.get(14)?,
        agent_visible: row.get(15)?,
        tools: tools_from_column(row.get(16)?),
    })
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
        (true, _) => format!(
            "SELECT {SKILL_COLUMNS} FROM skills WHERE agent_id = ?1 AND is_active = 1 AND archived = 0 ORDER BY usage_count DESC"
        ),
        (false, true) => format!(
            "SELECT {SKILL_COLUMNS} FROM skills WHERE agent_id = ?1 ORDER BY usage_count DESC"
        ),
        (false, false) => format!(
            "SELECT {SKILL_COLUMNS} FROM skills WHERE agent_id = ?1 AND archived = 0 ORDER BY usage_count DESC"
        ),
    };

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![agent_id], skill_row_from_row)?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn insert_skill(conn: &Connection, skill: &SkillRow) -> Result<()> {
    conn.execute(
        "INSERT INTO skills (id, agent_id, name, description, situation_pattern, guidance, source_type, source_context, file_path, effectiveness, usage_count, is_active, permission, archived, created_caller, agent_visible, tools, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
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
            skill.created_caller,
            skill.agent_visible,
            tools_to_column(&skill.tools),
            Utc::now().to_rfc3339(),
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

/// スキルを 1 回使ったと記録する（D-1146）。使った＝`read_skill` で本文を開いた、とする。
/// `usage_count` を +1、`last_used_at` を今にし、session があれば `skill_usage_log` に 1 行足す。
/// 同じターンで同じスキルを何度開いても、開いた回数だけ数える（実際に起きたことをそのまま数える）。
pub fn record_skill_use(
    conn: &Connection,
    agent_id: &str,
    skill_id: &str,
    session_id: Option<&str>,
) -> Result<()> {
    increment_skill_usage(conn, skill_id)?;
    if let Some(session_id) = session_id {
        insert_skill_usage(conn, agent_id, skill_id, session_id)?;
    }
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
        &format!(
            "SELECT {SKILL_COLUMNS} FROM skills WHERE agent_id = ?1 AND LOWER(name) = LOWER(?2) AND archived = 0 LIMIT 1"
        ),
        params![agent_id, name],
        skill_row_from_row,
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
        &format!(
            "SELECT {SKILL_COLUMNS} FROM skills WHERE agent_id = ?1 AND LOWER(name) = LOWER(?2) LIMIT 1"
        ),
        params![agent_id, name],
        skill_row_from_row,
    );

    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn find_skill_by_id(conn: &Connection, skill_id: &str) -> Result<Option<SkillRow>> {
    let result = conn.query_row(
        &format!("SELECT {SKILL_COLUMNS} FROM skills WHERE id = ?1 LIMIT 1"),
        params![skill_id],
        skill_row_from_row,
    );

    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn update_skill(conn: &Connection, skill: &SkillRow) -> Result<()> {
    // created_caller も書き戻す。create_my_skill の dedup 更新で「弱い caller が
    // 上書きしたら trust class を下げる」（昇格させない）ために更新経路でも反映する（#335）。
    //
    // agent_visible も書き戻す。オーナーの REST 更新経路（`api::skills::update_skill`）が
    // この列を切り替える口になる（#352）。エージェント側の経路（create_my_skill の dedup）は
    // 既存行を load-then-mutate して渡すため、この列は既存値がそのまま保存されるだけで
    // **agent が自分で true へ上げることはできない**（新規作成は false 既定）。オーナー等価で
    // ない caller が書き換えたら指定を外す（D-1143 `record_skill_rewrite`）。
    conn.execute(
        "UPDATE skills SET name = ?1, description = ?2, situation_pattern = ?3, guidance = ?4, is_active = ?5, archived = ?6, file_path = ?7, created_caller = ?8, agent_visible = ?9, tools = ?10, updated_at = ?11 WHERE id = ?12",
        params![
            skill.name,
            skill.description,
            skill.situation_pattern,
            skill.guidance,
            skill.is_active,
            skill.archived,
            skill.file_path,
            skill.created_caller,
            skill.agent_visible,
            tools_to_column(&skill.tools),
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

/// スキルが使われた（`read_skill` で本文が開かれた）ことをセッション単位で記録する。
/// 呼ぶのは `record_skill_use` だけ。
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
    let mut stmt = conn.prepare(&format!(
        "SELECT {SKILL_COLUMNS} FROM skills
         WHERE agent_id = ?1 AND usage_count = 0 AND archived = 0 AND created_at <= ?2
         ORDER BY created_at ASC"
    ))?;
    let rows = stmt.query_map(params![agent_id, cutoff], skill_row_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
