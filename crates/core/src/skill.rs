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
        })
    }

    /// Increment the usage count for a skill.
    pub fn increment_usage(&self, skill_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        queries::increment_skill_usage(&conn, skill_id)?;
        tracing::debug!(skill_id = %skill_id, "Incremented skill usage");
        Ok(())
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
        }
    }
}
