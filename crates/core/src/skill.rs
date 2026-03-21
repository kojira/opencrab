use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tracing;
use uuid::Uuid;

use opencrab_db::queries;

/// The origin of a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillSource {
    /// A built-in skill loaded from a file on disk.
    Standard {
        /// Path to the skill definition file.
        file_path: String,
    },
    /// A skill acquired at runtime through learning or experience.
    Acquired {
        /// How the skill was acquired (e.g., "conversation", "observation", "training").
        source_type: String,
        /// Additional context about the acquisition.
        source_context: String,
    },
}

/// Permission level required to use this skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SkillPermission {
    CoAgent,
    Agent,
    Owner,
}

impl Default for SkillPermission {
    fn default() -> Self {
        SkillPermission::Agent
    }
}

impl SkillPermission {
    pub fn from_db_str(s: &str) -> Self {
        match s.trim_matches('"') {
            "owner" => SkillPermission::Owner,
            "co_agent" | "co-agent" | "coagent" => SkillPermission::CoAgent,
            _ => SkillPermission::Agent,
        }
    }

    pub fn as_db_str(&self) -> &str {
        match self {
            SkillPermission::Owner => "\"owner\"",
            SkillPermission::CoAgent => "\"co_agent\"",
            SkillPermission::Agent => "\"agent\"",
        }
    }
}

/// A skill that an agent can use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Unique skill identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Description of what this skill does.
    pub description: String,
    /// Version string for the skill.
    pub version: String,
    /// Actions this skill provides (action names).
    pub actions: Vec<String>,
    /// Guidance text for the LLM on how to use this skill.
    pub guidance: String,
    /// Where this skill came from.
    pub source: SkillSource,
    /// How many times this skill has been used.
    pub usage_count: i32,
    /// Effectiveness score (0.0 to 1.0), if evaluated.
    pub effectiveness: Option<f64>,
    /// Permission level required to use this skill.
    pub permission: SkillPermission,
}

/// Manages skills for an agent.
///
/// Skills represent capabilities the agent can invoke. They can be standard
/// (loaded from configuration) or acquired during runtime.
#[derive(Debug, Clone)]
pub struct SkillManager {
    agent_id: String,
    conn: Arc<Mutex<Connection>>,
}

impl SkillManager {
    /// Create a new SkillManager for the given agent.
    pub fn new(agent_id: impl Into<String>, conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            agent_id: agent_id.into(),
            conn,
        }
    }

    /// Get all active skills for this agent.
    pub fn get_active_skills(&self) -> Result<Vec<Skill>> {
        let conn = self.conn.lock().unwrap();
        let rows = queries::list_skills(&conn, &self.agent_id, true)?;
        Ok(rows.into_iter().map(Self::row_to_skill).collect())
    }

    /// Acquire a new skill at runtime.
    pub fn acquire_skill(
        &self,
        name: &str,
        description: &str,
        guidance: &str,
        source_type: &str,
        source_context: &str,
    ) -> Result<Skill> {
        let id = Uuid::new_v4().to_string();
        let conn = self.conn.lock().unwrap();

        let row = queries::SkillRow {
            id: id.clone(),
            agent_id: self.agent_id.clone(),
            name: name.to_string(),
            description: description.to_string(),
            situation_pattern: String::new(),
            guidance: guidance.to_string(),
            source_type: "acquired".to_string(),
            source_context: Some(source_context.to_string()),
            file_path: None,
            effectiveness: None,
            usage_count: 0,
            is_active: true,
            permission: SkillPermission::Agent.as_db_str().to_string(),
            archived: false,
        };

        queries::insert_skill(&conn, &row)?;

        tracing::info!(
            agent_id = %self.agent_id,
            skill_name = %name,
            source_type = %source_type,
            "Acquired new skill"
        );

        Ok(Skill {
            id,
            name: name.to_string(),
            description: description.to_string(),
            version: "1.0.0".to_string(),
            actions: Vec::new(),
            guidance: guidance.to_string(),
            source: SkillSource::Acquired {
                source_type: source_type.to_string(),
                source_context: source_context.to_string(),
            },
            usage_count: 0,
            effectiveness: None,
            permission: SkillPermission::Agent,
        })
    }

    /// Increment the usage count for a skill.
    pub fn increment_usage(&self, skill_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        queries::increment_skill_usage(&conn, skill_id)?;
        tracing::debug!(skill_id = %skill_id, "Incremented skill usage");
        Ok(())
    }

    /// Acquire a skill with deduplication (upsert behavior).
    /// If a skill with the same name exists, update it instead of creating a new one.
    pub fn acquire_skill_dedup(
        &self,
        name: &str,
        description: &str,
        guidance: &str,
        source_type: &str,
        source_context: &str,
    ) -> Result<Skill> {
        let conn = self.conn.lock().unwrap();

        if let Some(existing) = queries::find_skill_by_name(&conn, &self.agent_id, name)? {
            let mut updated = existing;
            updated.description = description.to_string();
            updated.guidance = guidance.to_string();
            queries::update_skill(&conn, &updated)?;

            tracing::info!(
                agent_id = %self.agent_id,
                skill_name = %name,
                "Updated existing skill (dedup)"
            );

            Ok(Self::row_to_skill(updated))
        } else {
            drop(conn);
            self.acquire_skill(name, description, guidance, source_type, source_context)
        }
    }

    /// Archive a skill (logical deletion).
    pub fn archive_skill(&self, skill_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        queries::archive_skill(&conn, skill_id, true)?;
        tracing::info!(skill_id = %skill_id, "Archived skill");
        Ok(())
    }

    /// Restore an archived skill.
    pub fn restore_skill(&self, skill_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        queries::archive_skill(&conn, skill_id, false)?;
        tracing::info!(skill_id = %skill_id, "Restored skill");
        Ok(())
    }

    /// Merge two skills: combine usage counts, delete source.
    pub fn merge_skills(&self, source_id: &str, target_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        queries::merge_skills(&conn, source_id, target_id)?;
        tracing::info!(
            source_id = %source_id,
            target_id = %target_id,
            "Merged skills"
        );
        Ok(())
    }

    /// Check for duplicate skills and log them.
    pub fn check_and_cleanup_duplicates(&self) -> Result<(usize, usize)> {
        let conn = self.conn.lock().unwrap();
        let duplicates = queries::find_duplicate_skills(&conn, &self.agent_id)?;
        let dup_count = duplicates.len();

        if dup_count > 0 {
            tracing::info!(
                agent_id = %self.agent_id,
                duplicate_count = dup_count,
                "Found duplicate skills"
            );
        }

        let unused = queries::find_unused_skills(&conn, &self.agent_id, 7)?;
        let unused_count = unused.len();

        if unused_count > 0 {
            tracing::info!(
                agent_id = %self.agent_id,
                unused_count = unused_count,
                "Found unused skills (7+ days old)"
            );
        }

        Ok((dup_count, unused_count))
    }

    /// Build a context string describing available skills for LLM prompts.
    pub fn build_context(&self) -> Result<String> {
        let skills = self.get_active_skills()?;
        if skills.is_empty() {
            return Ok(String::new());
        }

        let mut ctx = String::from("## Available Skills\n\n");

        for skill in &skills {
            ctx.push_str(&format!("### {} (used {} times)\n", skill.name, skill.usage_count));
            ctx.push_str(&format!("{}\n", skill.description));

            if !skill.actions.is_empty() {
                ctx.push_str(&format!("Actions: {}\n", skill.actions.join(", ")));
            }

            if !skill.guidance.is_empty() {
                ctx.push_str(&format!("Guidance: {}\n", skill.guidance));
            }

            ctx.push('\n');
        }

        Ok(ctx)
    }

    /// Convert a database row into a Skill struct.
    fn row_to_skill(row: queries::SkillRow) -> Skill {
        let source = if row.source_type == "standard" {
            SkillSource::Standard {
                file_path: row.file_path.unwrap_or_default(),
            }
        } else {
            SkillSource::Acquired {
                source_type: row.source_type,
                source_context: row.source_context.unwrap_or_default(),
            }
        };

        // Parse actions from situation_pattern field (stored as comma-separated or JSON).
        let actions: Vec<String> = if row.situation_pattern.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&row.situation_pattern).unwrap_or_else(|_| {
                row.situation_pattern
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
        };

        Skill {
            id: row.id,
            name: row.name,
            description: row.description,
            version: "1.0.0".to_string(),
            actions,
            guidance: row.guidance,
            source,
            usage_count: row.usage_count,
            effectiveness: row.effectiveness,
            permission: SkillPermission::from_db_str(&row.permission),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_sm() -> SkillManager {
        let conn = opencrab_db::init_memory().unwrap();
        SkillManager::new("agent-test", Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn test_acquire_skill() {
        let sm = test_sm();
        let skill = sm
            .acquire_skill("coding", "Write code", "Use best practices", "training", "initial setup")
            .unwrap();
        assert_eq!(skill.name, "coding");
        assert_eq!(skill.description, "Write code");
        assert_eq!(skill.usage_count, 0);
    }

    #[test]
    fn test_get_active_empty() {
        let sm = test_sm();
        let skills = sm.get_active_skills().unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_get_active_with_skills() {
        let sm = test_sm();
        sm.acquire_skill("skill-a", "desc a", "guide a", "training", "ctx").unwrap();
        sm.acquire_skill("skill-b", "desc b", "guide b", "training", "ctx").unwrap();
        let skills = sm.get_active_skills().unwrap();
        assert_eq!(skills.len(), 2);
    }

    #[test]
    fn test_increment_usage() {
        let sm = test_sm();
        let skill = sm
            .acquire_skill("coding", "Write code", "guide", "training", "ctx")
            .unwrap();
        sm.increment_usage(&skill.id).unwrap();
        let skills = sm.get_active_skills().unwrap();
        let found = skills.iter().find(|s| s.id == skill.id).unwrap();
        assert_eq!(found.usage_count, 1);
    }

    #[test]
    fn test_build_context() {
        let sm = test_sm();
        sm.acquire_skill("my-skill", "does things", "do it well", "training", "ctx")
            .unwrap();
        let ctx = sm.build_context().unwrap();
        assert!(ctx.contains("Available Skills"));
        assert!(ctx.contains("my-skill"));
    }
}
