//! 台帳 `skills` 書き（DESIGN-DASHBOARD-P2 SLICE 4）。旧 `skills` は書かない。
//!
//! state は assemble-skill-v1 の閉表: archived → archived、
//! !archived && active → active、両方 0 → retired。

use std::path::Path;

use crate::Store;
use opencrab_port::SubjectId;
use rusqlite::{params, OptionalExtension};

const DEFAULT_PERMISSION: &str = "\"agent\"";

#[derive(Debug)]
pub enum SkillCommandError {
    Store(rusqlite::Error),
    AgentMissing,
    SkillsDir(String),
}

impl From<rusqlite::Error> for SkillCommandError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for SkillCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "{error}"),
            Self::AgentMissing => write!(f, "agent not found"),
            Self::SkillsDir(detail) => write!(f, "{detail}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SkillView {
    pub skill_id: String,
    pub owner_subject_id: SubjectId,
    pub name: String,
    pub description: String,
    pub situation_pattern: String,
    pub guidance: String,
    pub source_type: String,
    pub source_context: Option<String>,
    pub source_relative_path: Option<String>,
    pub effectiveness: Option<f64>,
    pub usage_count: i64,
    pub active: bool,
    pub permission: String,
    pub archived: bool,
    pub created_by_principal: Option<String>,
    pub visible_to_agent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillCreate {
    pub name: String,
    pub description: String,
    pub situation_pattern: String,
    pub guidance: String,
    pub permission: Option<String>,
    pub visible_to_agent: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub guidance: Option<String>,
    pub situation_pattern: Option<String>,
    pub visible_to_agent: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillSeedResult {
    pub seeded: Vec<String>,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

fn skill_state(active: bool, archived: bool) -> &'static str {
    if archived {
        "archived"
    } else if active {
        "active"
    } else {
        "retired"
    }
}

struct ParsedSkill {
    name: String,
    description: String,
    permission_db: String,
    actions: Vec<String>,
    body: String,
}

/// 本体 `setup.rs` の `parse_skill_md` と同じ限定 frontmatter。
fn parse_skill_md(content: &str) -> Option<ParsedSkill> {
    let content = content.trim_start_matches('\u{feff}');
    let mut lines = content.lines();
    let opened = loop {
        match lines.next() {
            Some(line) if line.trim().is_empty() => continue,
            Some(line) => break line.trim() == "---",
            None => return None,
        }
    };
    if !opened {
        return None;
    }
    let mut fm_lines = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        fm_lines.push(line);
    }
    if !closed {
        return None;
    }
    let body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    let mut name = None;
    let mut description = None;
    let mut permission = None;
    let mut actions = Vec::new();
    let mut in_actions = false;
    for line in fm_lines {
        if line.starts_with([' ', '\t']) {
            let trimmed = line.trim();
            if in_actions {
                if let Some(item) = trimmed.strip_prefix("- ") {
                    let value = item.trim().trim_matches('"').trim().to_string();
                    if !value.is_empty() {
                        actions.push(value);
                    }
                }
            }
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        in_actions = false;
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim().to_string();
        match key {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "permission" => permission = Some(value),
            "actions" => in_actions = true,
            _ => {}
        }
    }
    let name = name.filter(|value| !value.is_empty())?;
    let perm = match permission.as_deref().unwrap_or("agent") {
        "owner" => "owner",
        "co_agent" | "co-agent" | "coagent" => "co_agent",
        _ => "agent",
    };
    Some(ParsedSkill {
        name,
        description: description.unwrap_or_default(),
        permission_db: format!("\"{perm}\""),
        actions,
        body,
    })
}

fn map_skill(row: &rusqlite::Row<'_>) -> rusqlite::Result<SkillView> {
    Ok(SkillView {
        skill_id: row.get(0)?,
        owner_subject_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        situation_pattern: row.get(4)?,
        guidance: row.get(5)?,
        source_type: row.get(6)?,
        source_context: row.get(7)?,
        source_relative_path: row.get(8)?,
        effectiveness: row.get(9)?,
        usage_count: row.get(10)?,
        active: row.get::<_, i64>(11)? != 0,
        permission: row.get(12)?,
        archived: row.get::<_, i64>(13)? != 0,
        created_by_principal: row.get(14)?,
        visible_to_agent: row.get::<_, i64>(15)? != 0,
    })
}

const SKILL_COLUMNS: &str =
    "skill_id,owner_subject_id,name,description,situation_pattern,guidance,\
 source_type,source_context,source_relative_path,effectiveness,usage_count,active,permission,\
 archived,created_by_principal,visible_to_agent";

impl Store {
    pub fn skill_list(
        &self,
        owner: SubjectId,
        active_only: bool,
        include_archived: bool,
    ) -> std::result::Result<Vec<SkillView>, SkillCommandError> {
        let sql = match (active_only, include_archived) {
            (true, _) => format!(
                "SELECT {SKILL_COLUMNS} FROM skills
                 WHERE owner_subject_id=?1 AND active=1 AND archived=0
                 ORDER BY usage_count DESC"
            ),
            (false, true) => format!(
                "SELECT {SKILL_COLUMNS} FROM skills
                 WHERE owner_subject_id=?1 ORDER BY usage_count DESC"
            ),
            (false, false) => format!(
                "SELECT {SKILL_COLUMNS} FROM skills
                 WHERE owner_subject_id=?1 AND archived=0
                 ORDER BY usage_count DESC"
            ),
        };
        let conn = self.c();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![owner], map_skill)?
            .collect::<crate::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn skill_create(
        &self,
        owner: SubjectId,
        create: &SkillCreate,
        now: i64,
    ) -> std::result::Result<String, SkillCommandError> {
        let skill_id = uuid::Uuid::new_v4().to_string();
        let permission = create.permission.as_deref().unwrap_or(DEFAULT_PERMISSION);
        let visible = create.visible_to_agent.unwrap_or(false);
        let definition: &[u8] = b"{}";
        self.c().execute(
            "INSERT INTO skills(
               skill_id,owner_subject_id,name,description,situation_pattern,guidance,
               permission,visible_to_agent,active,archived,state,definition,source_type,
               source_bytes,source_context,source_relative_path,created_by_principal,
               effectiveness,last_used_at,revision,usage_count,created_at,updated_at
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,0,'active',?9,'manual',NULL,NULL,NULL,NULL,NULL,NULL,1,0,?10,?10)",
            params![
                skill_id,
                owner,
                create.name,
                create.description,
                create.situation_pattern,
                create.guidance,
                permission,
                i64::from(visible),
                definition,
                now
            ],
        )?;
        Ok(skill_id)
    }

    pub fn skill_update(
        &self,
        owner: SubjectId,
        skill_id: &str,
        patch: &SkillPatch,
        now: i64,
    ) -> std::result::Result<bool, SkillCommandError> {
        let conn = self.c();
        let existing = conn
            .query_row(
                &format!(
                    "SELECT {SKILL_COLUMNS} FROM skills WHERE skill_id=?1 AND owner_subject_id=?2"
                ),
                params![skill_id, owner],
                map_skill,
            )
            .optional()?;
        let Some(existing) = existing else {
            return Ok(false);
        };
        let name = patch.name.as_deref().unwrap_or(&existing.name);
        let description = patch
            .description
            .as_deref()
            .unwrap_or(&existing.description);
        let guidance = patch.guidance.as_deref().unwrap_or(&existing.guidance);
        let situation_pattern = patch
            .situation_pattern
            .as_deref()
            .unwrap_or(&existing.situation_pattern);
        let visible = patch.visible_to_agent.unwrap_or(existing.visible_to_agent);
        conn.execute(
            "UPDATE skills SET name=?1,description=?2,situation_pattern=?3,guidance=?4,
             visible_to_agent=?5,updated_at=?6 WHERE skill_id=?7",
            params![
                name,
                description,
                situation_pattern,
                guidance,
                i64::from(visible),
                now,
                skill_id
            ],
        )?;
        Ok(true)
    }

    pub fn skill_set_active(
        &self,
        skill_id: &str,
        active: bool,
        now: i64,
    ) -> std::result::Result<(), SkillCommandError> {
        let conn = self.c();
        let archived: Option<i64> = conn
            .query_row(
                "SELECT archived FROM skills WHERE skill_id=?1",
                params![skill_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(archived) = archived else {
            return Ok(());
        };
        conn.execute(
            "UPDATE skills SET active=?1,state=?2,updated_at=?3 WHERE skill_id=?4",
            params![
                i64::from(active),
                skill_state(active, archived != 0),
                now,
                skill_id
            ],
        )?;
        Ok(())
    }

    pub fn skill_archive(
        &self,
        skill_id: &str,
        archived: bool,
        now: i64,
    ) -> std::result::Result<(), SkillCommandError> {
        let conn = self.c();
        let active: Option<i64> = conn
            .query_row(
                "SELECT active FROM skills WHERE skill_id=?1",
                params![skill_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(active) = active else {
            return Ok(());
        };
        conn.execute(
            "UPDATE skills SET archived=?1,state=?2,updated_at=?3 WHERE skill_id=?4",
            params![
                i64::from(archived),
                skill_state(active != 0, archived),
                now,
                skill_id
            ],
        )?;
        Ok(())
    }

    pub fn skill_find_by_name(
        &self,
        owner: SubjectId,
        name: &str,
    ) -> std::result::Result<Option<SkillView>, SkillCommandError> {
        let row = self
            .c()
            .query_row(
                &format!(
                    "SELECT {SKILL_COLUMNS} FROM skills
                     WHERE owner_subject_id=?1 AND LOWER(name)=LOWER(?2) AND archived=0 LIMIT 1"
                ),
                params![owner, name],
                map_skill,
            )
            .optional()?;
        Ok(row)
    }

    pub fn skill_seed_standard(
        &self,
        owner: SubjectId,
        skills_dir: &Path,
        now: i64,
    ) -> std::result::Result<SkillSeedResult, SkillCommandError> {
        let exists: Option<i64> = self
            .c()
            .query_row(
                "SELECT id FROM subjects WHERE id=?1",
                params![owner],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(SkillCommandError::AgentMissing);
        }
        let entries = std::fs::read_dir(skills_dir).map_err(|error| {
            SkillCommandError::SkillsDir(format!(
                "スキルディレクトリを読めません（{}）: {error}",
                skills_dir.display()
            ))
        })?;
        let mut seeded = Vec::new();
        let mut skipped = Vec::new();
        let mut errors = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let is_skill_md = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".skill.md"));
            if !is_skill_md {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(content) => content,
                Err(error) => {
                    errors.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            let Some(parsed) = parse_skill_md(&content) else {
                errors.push(format!(
                    "{}: frontmatter を解釈できませんでした",
                    path.display()
                ));
                continue;
            };
            match self.skill_find_by_name(owner, &parsed.name) {
                Ok(Some(_)) => {
                    skipped.push(parsed.name);
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    errors.push(format!("{}: {error}", parsed.name));
                    continue;
                }
            }
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let name = parsed.name.clone();
            let situation_pattern = match serde_json::to_string(&parsed.actions) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(format!("{}: {error}", parsed.name));
                    continue;
                }
            };
            let skill_id = uuid::Uuid::new_v4().to_string();
            let source_bytes = content.as_bytes();
            let source_path = format!("skills/{file_name}");
            if let Err(error) = self.c().execute(
                "INSERT INTO skills(
                   skill_id,owner_subject_id,name,description,situation_pattern,guidance,
                   permission,visible_to_agent,active,archived,state,definition,source_type,
                   source_bytes,source_context,source_relative_path,created_by_principal,
                   effectiveness,last_used_at,revision,usage_count,created_at,updated_at
                 ) VALUES(?1,?2,?3,?4,?5,?6,?7,0,1,0,'active',?8,'standard',?8,NULL,?9,NULL,NULL,NULL,1,0,?10,?10)",
                params![
                    skill_id,
                    owner,
                    name,
                    parsed.description,
                    situation_pattern,
                    parsed.body,
                    parsed.permission_db,
                    source_bytes,
                    source_path,
                    now
                ],
            ) {
                errors.push(format!("{}: {error}", parsed.name));
                continue;
            }
            seeded.push(parsed.name);
        }
        Ok(SkillSeedResult {
            seeded,
            skipped,
            errors,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_port::{Standing, SubjectKind};

    fn store_with_agent() -> (Store, SubjectId) {
        let store = Store::new_in_memory().unwrap();
        let id = store
            .create_subject(
                SubjectKind::Agent,
                "Ada",
                "You are Ada.",
                "engine",
                Standing::Trusted,
                1,
            )
            .unwrap();
        (store, id)
    }

    #[test]
    fn parse_frontmatter_matches_body() {
        let md = "---\nname: autonomous\ndescription: \"自律モード - 説明\"\nversion: 1\npermission: agent\nactions:\n  - send_speech\n  - declare_done\n---\n\n# 本文\n\nガイダンス本体。\n";
        let parsed = parse_skill_md(md).expect("parse");
        assert_eq!(parsed.name, "autonomous");
        assert_eq!(parsed.description, "自律モード - 説明");
        assert_eq!(parsed.permission_db, "\"agent\"");
        assert_eq!(parsed.actions, vec!["send_speech", "declare_done"]);
        assert!(parsed.body.contains("ガイダンス本体"));
    }

    #[test]
    fn create_list_update_toggle_archive_restore() {
        let (store, owner) = store_with_agent();
        let id = store
            .skill_create(
                owner,
                &SkillCreate {
                    name: "Deploy".into(),
                    description: "ship it".into(),
                    situation_pattern: "when shipping".into(),
                    guidance: "do the thing".into(),
                    permission: None,
                    visible_to_agent: Some(true),
                },
                10,
            )
            .unwrap();
        let listed = store.skill_list(owner, false, false).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Deploy");
        assert_eq!(listed[0].permission, DEFAULT_PERMISSION);
        assert!(listed[0].active);
        assert!(!listed[0].archived);
        assert!(listed[0].visible_to_agent);

        assert!(store
            .skill_update(
                owner,
                &id,
                &SkillPatch {
                    name: Some("Ship".into()),
                    ..SkillPatch::default()
                },
                11,
            )
            .unwrap());
        assert!(!store
            .skill_update(owner, "missing", &SkillPatch::default(), 12)
            .unwrap());

        store.skill_set_active(&id, false, 13).unwrap();
        store.skill_archive(&id, true, 14).unwrap();
        let hidden = store.skill_list(owner, false, false).unwrap();
        assert!(hidden.is_empty());
        let archived = store.skill_list(owner, false, true).unwrap();
        assert_eq!(archived[0].name, "Ship");
        assert!(archived[0].archived);
        assert!(!archived[0].active);

        store.skill_archive(&id, false, 15).unwrap();
        let restored = store.skill_list(owner, false, false).unwrap();
        assert_eq!(restored.len(), 1);
        assert!(!restored[0].archived);
    }

    #[test]
    fn seed_standard_is_idempotent() {
        let (store, owner) = store_with_agent();
        let dir = std::env::temp_dir().join(format!("opencrab-skill-seed-{}", owner));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("autonomous.skill.md"),
            "---\nname: autonomous\ndescription: d\npermission: owner\nactions:\n  - send_speech\n---\nbody\n",
        )
        .unwrap();
        let first = store.skill_seed_standard(owner, &dir, 20).unwrap();
        assert_eq!(first.seeded, vec!["autonomous"]);
        assert!(first.skipped.is_empty());
        let second = store.skill_seed_standard(owner, &dir, 21).unwrap();
        assert!(second.seeded.is_empty());
        assert_eq!(second.skipped, vec!["autonomous"]);
        let listed = store.skill_list(owner, true, false).unwrap();
        assert_eq!(listed[0].source_type, "standard");
        assert_eq!(listed[0].permission, "\"owner\"");
        assert_eq!(listed[0].situation_pattern, "[\"send_speech\"]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_missing_agent_is_error() {
        let store = Store::new_in_memory().unwrap();
        let err = store
            .skill_seed_standard(99, Path::new("skills"), 1)
            .err()
            .unwrap();
        assert!(matches!(err, SkillCommandError::AgentMissing));
    }
}
