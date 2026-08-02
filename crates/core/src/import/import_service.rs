use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::import::openclaw_parser::{ScanResult, SkillImportData};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportOptions {
    pub overwrite_if_exists: bool,
    pub agent_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportCounts {
    pub soul: bool,
    pub identity: bool,
    pub memory_curated: usize,
    pub daily_logs: usize,
    pub skills: usize,
    pub scripts_copied: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub agent_id: String,
    pub counts: ImportCounts,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub indexed_logs_count: usize,
}

pub fn execute_import(
    conn: &Connection,
    agent_id: &str,
    scan_result: &ScanResult,
    options: &ImportOptions,
) -> Result<ImportResult> {
    let mut warnings = scan_result.warnings.clone();
    let mut errors = Vec::new();
    let mut counts = ImportCounts {
        soul: false,
        identity: false,
        memory_curated: 0,
        daily_logs: 0,
        skills: 0,
        scripts_copied: 0,
    };

    let existing = opencrab_db::queries::get_agent(conn, agent_id)?;

    let mut row = existing
        .clone()
        .unwrap_or_else(|| opencrab_db::queries::AgentRow {
            agent_id: agent_id.to_string(),
            name: String::new(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: String::new(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        });

    if scan_result.soul.found {
        if existing.is_some() && !options.overwrite_if_exists {
            warnings.push(
                "Agent already exists, skipping soul (overwrite_if_exists=false)".to_string(),
            );
        } else {
            row.persona_name = scan_result.soul.persona_name.clone();
            row.personality = Some(scan_result.soul.personality.clone());
            row.instructions = scan_result.instructions.clone();
            counts.soul = true;
        }
    }

    if scan_result.identity.found {
        if existing.is_some() && !options.overwrite_if_exists {
            warnings.push(
                "Agent already exists, skipping identity (overwrite_if_exists=false)".to_string(),
            );
        } else {
            row.name = options
                .agent_name
                .clone()
                .unwrap_or_else(|| scan_result.identity.name.clone());
            row.image_url = scan_result.identity.image_url.clone();
            row.metadata_json = Some(scan_result.identity.metadata_json.clone());
            counts.identity = true;
        }
    }

    if counts.soul || counts.identity {
        if row.name.is_empty() {
            row.name = row.persona_name.clone();
        }
        if row.persona_name.is_empty() {
            row.persona_name = row.name.clone();
        }
        opencrab_db::queries::upsert_agent(conn, &row)?;
    }

    // Memory curated (insert in transaction)
    let tx_result: Result<()> = (|| {
        for mem in &scan_result.memory_curated {
            let row = opencrab_db::queries::CuratedMemoryRow {
                id: Uuid::new_v4().to_string(),
                agent_id: agent_id.to_string(),
                category: mem.category.clone(),
                content: mem.content.clone(),
                created_at: String::new(),
            };
            opencrab_db::queries::upsert_curated_memory(conn, &row)?;
            counts.memory_curated += 1;
        }

        for log in &scan_result.daily_logs {
            let row = opencrab_db::queries::CuratedMemoryRow {
                id: Uuid::new_v4().to_string(),
                agent_id: agent_id.to_string(),
                category: log.category.clone(),
                content: log.content.clone(),
                created_at: String::new(),
            };
            opencrab_db::queries::upsert_curated_memory(conn, &row)?;
            counts.daily_logs += 1;
        }

        Ok(())
    })();

    if let Err(e) = tx_result {
        errors.push(format!("Memory import error: {}", e));
    }

    // Skills
    for skill_data in &scan_result.skills {
        match import_skill(conn, agent_id, skill_data, options.overwrite_if_exists) {
            Ok(imported) => {
                if imported {
                    counts.skills += 1;
                    counts.scripts_copied += skill_data.script_files.len();
                } else {
                    warnings.push(format!(
                        "Skill '{}' already exists, skipping",
                        skill_data.name
                    ));
                }
            }
            Err(e) => {
                errors.push(format!("Skill '{}' import error: {}", skill_data.name, e));
            }
        }
    }

    Ok(ImportResult {
        agent_id: agent_id.to_string(),
        counts,
        warnings,
        errors,
        indexed_logs_count: 0,
    })
}

fn import_skill(
    conn: &Connection,
    agent_id: &str,
    skill: &SkillImportData,
    overwrite: bool,
) -> Result<bool> {
    let existing = opencrab_db::queries::find_skill_by_name(conn, agent_id, &skill.name)?;

    if existing.is_some() && !overwrite {
        return Ok(false);
    }

    if let Some(existing) = existing {
        // Update existing skill
        let updated = opencrab_db::queries::SkillRow {
            id: existing.id,
            agent_id: agent_id.to_string(),
            name: skill.name.clone(),
            description: skill.description.clone(),
            situation_pattern: skill.situation_pattern.clone(),
            guidance: skill.guidance.clone(),
            source_type: skill.source_type.clone(),
            source_context: skill.source_context.clone(),
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: "agent".to_string(),
            archived: false,
            // #335: import はオーナーのセットアップ由来。None = legacy grandfather（Owner 相当）。
            created_caller: None,
        };
        opencrab_db::queries::update_skill(conn, &updated)?;
    } else {
        let row = opencrab_db::queries::SkillRow {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            name: skill.name.clone(),
            description: skill.description.clone(),
            situation_pattern: skill.situation_pattern.clone(),
            guidance: skill.guidance.clone(),
            source_type: skill.source_type.clone(),
            source_context: skill.source_context.clone(),
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: "agent".to_string(),
            archived: false,
            // #335: import はオーナーのセットアップ由来。None = legacy grandfather（Owner 相当）。
            created_caller: None,
        };
        opencrab_db::queries::insert_skill(conn, &row)?;
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> rusqlite::Connection {
        opencrab_db::init_memory().unwrap()
    }

    #[test]
    fn test_execute_import_empty_scan() {
        let conn = test_conn();
        let agent_id = "test-agent-import-001";
        let scan = crate::import::openclaw_parser::ScanResult {
            source_dir: "/tmp".to_string(),
            soul: crate::import::openclaw_parser::SoulImportData {
                persona_name: "テストボット".to_string(),
                personality: "test personality".to_string(),
                found: true,
            },
            identity: crate::import::openclaw_parser::IdentityImportData {
                name: "テストボット".to_string(),
                image_url: None,
                metadata_json: "{}".to_string(),
                found: true,
            },
            memory_curated: vec![],
            instructions: String::new(),
            skills: vec![],
            daily_logs: vec![],
            warnings: vec![],
            excluded: vec![],
        };
        let opts = ImportOptions {
            overwrite_if_exists: true,
            agent_name: Some("テストボット".to_string()),
        };
        let result = execute_import(&conn, agent_id, &scan, &opts).unwrap();
        assert_eq!(result.agent_id, agent_id);
        assert!(result.counts.soul);
        assert!(result.counts.identity);
        assert_eq!(result.counts.memory_curated, 0);
    }

    #[test]
    fn test_execute_import_with_memory_and_skills() {
        let conn = test_conn();
        let agent_id = "test-agent-import-002";
        let scan = crate::import::openclaw_parser::ScanResult {
            source_dir: "/tmp".to_string(),
            soul: crate::import::openclaw_parser::SoulImportData {
                persona_name: "テストボット".to_string(),
                personality: "personality text".to_string(),
                found: true,
            },
            identity: crate::import::openclaw_parser::IdentityImportData {
                name: "テストボット".to_string(),
                image_url: None,
                metadata_json: "{}".to_string(),
                found: true,
            },
            memory_curated: vec![crate::import::openclaw_parser::MemoryCuratedImportData {
                category: "long_term/テスト".to_string(),
                content: "test content".to_string(),
            }],
            instructions: String::new(),
            skills: vec![crate::import::openclaw_parser::SkillImportData {
                name: "test-skill".to_string(),
                description: "A test skill".to_string(),
                situation_pattern: "A test skill".to_string(),
                guidance: "Do the test".to_string(),
                source_type: "openclaw_import".to_string(),
                source_context: Some("/path/to/skill".to_string()),
                script_files: vec![],
            }],
            daily_logs: vec![],
            warnings: vec![],
            excluded: vec![],
        };
        let opts = ImportOptions {
            overwrite_if_exists: true,
            agent_name: None,
        };
        let result = execute_import(&conn, agent_id, &scan, &opts).unwrap();
        assert_eq!(result.counts.memory_curated, 1);
        assert_eq!(result.counts.skills, 1);
    }
}
