use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::heartbeat::HeartbeatConfig;
use crate::identity::Identity;
use crate::memory::MemoryManager;
use crate::skill::SkillManager;
use crate::soul::Soul;
use crate::workspace::Workspace;

use opencrab_db::queries;

/// Reference to a specific LLM provider and model combination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRef {
    /// Provider name (e.g., "openai", "anthropic", "ollama").
    pub provider: String,
    /// Model identifier (e.g., "gpt-4o", "claude-3-opus").
    pub model: String,
}

/// Model assignments for different task types.
///
/// Each field is optional; when `None`, the default model is used.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentModels {
    /// Model for deep thinking and reasoning.
    pub thinking: Option<ModelRef>,
    /// Model for conversational responses.
    pub conversation: Option<ModelRef>,
    /// Model for analysis and evaluation tasks.
    pub analysis: Option<ModelRef>,
    /// Model for function/tool calling.
    pub tool_calling: Option<ModelRef>,
    /// Model for generating embeddings.
    pub embedding: Option<ModelRef>,
}

/// LLM configuration for an agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLlmConfig {
    /// Default LLM provider to use.
    pub default_provider: String,
    /// Default model identifier.
    pub default_model: String,
    /// Task-specific model assignments.
    pub models: AgentModels,
    /// Whether the agent can dynamically select models based on task complexity.
    pub allow_self_selection: bool,
    /// Models the agent is allowed to select from (when self-selection is enabled).
    pub selectable_models: Vec<ModelRef>,
}

impl Default for AgentLlmConfig {
    fn default() -> Self {
        Self {
            default_provider: "openai".to_string(),
            default_model: "gpt-4o-mini".to_string(),
            models: AgentModels::default(),
            allow_self_selection: false,
            selectable_models: Vec::new(),
        }
    }
}

/// The main Agent struct, combining all components.
///
/// An Agent is the central entity in the OpenCrab framework. It has a soul
/// (personality), identity (role), memories, skills, a workspace, LLM config,
/// and a heartbeat configuration.
#[derive(Debug)]
pub struct Agent {
    /// Unique agent identifier.
    pub id: String,
    /// The agent's soul (personality and values).
    pub soul: Soul,
    /// The agent's identity (name, role).
    pub identity: Identity,
    /// Memory manager for curated and session memories.
    pub memory: MemoryManager,
    /// Skill manager for available capabilities.
    pub skills: SkillManager,
    /// Sandboxed workspace for file operations.
    pub workspace: Workspace,
    /// LLM configuration.
    pub llm_config: AgentLlmConfig,
    /// Heartbeat configuration.
    pub heartbeat: HeartbeatConfig,
}

impl Agent {
    /// Create a new Agent with the given configuration.
    ///
    /// # Arguments
    /// * `id` - Unique agent identifier.
    /// * `soul` - The agent's personality and values.
    /// * `identity` - The agent's role and name.
    /// * `conn` - Shared database connection.
    /// * `workspace_root` - Path to the workspace directory.
    /// * `llm_config` - LLM configuration.
    /// * `heartbeat` - Heartbeat configuration.
    pub fn new(
        id: impl Into<String>,
        soul: Soul,
        identity: Identity,
        conn: opencrab_db::Db,
        workspace_root: impl Into<std::path::PathBuf>,
        llm_config: AgentLlmConfig,
        heartbeat: HeartbeatConfig,
    ) -> Result<Self> {
        let id = id.into();
        let memory = MemoryManager::new(&id, conn.clone());
        let skills = SkillManager::new(&id, conn);
        let workspace = Workspace::from_root(workspace_root)?;

        Ok(Self {
            id,
            soul,
            identity,
            memory,
            skills,
            workspace,
            llm_config,
            heartbeat,
        })
    }

    /// Load an Agent from the database.
    ///
    /// Reads the soul and identity from the DB, creating a fully initialized
    /// agent with managers for memory, skills, and workspace.
    pub fn load(
        agent_id: &str,
        conn: opencrab_db::Db,
        workspace_root: impl Into<std::path::PathBuf>,
        llm_config: AgentLlmConfig,
        heartbeat: HeartbeatConfig,
    ) -> Result<Self> {
        let (soul, identity) = {
            let db = conn.lock().unwrap();

            let row = queries::get_agent(&db, agent_id)?
                .with_context(|| format!("Agent not found for agent: {}", agent_id))?;

            let soul = Soul {
                persona_name: row.persona_name,
                thinking_style: Default::default(),
                custom_traits: row.personality.and_then(|s| serde_json::from_str(&s).ok()),
            };

            let identity = Identity {
                agent_id: row.agent_id,
                name: row.name,
                job_title: row.job_title,
                organization: row.organization,
                image_url: row.image_url,
            };

            (soul, identity)
        };

        Self::new(
            agent_id,
            soul,
            identity,
            conn,
            workspace_root,
            llm_config,
            heartbeat,
        )
    }

    /// Build the full context string for LLM prompts.
    ///
    /// Combines soul, identity, memory, and skills contexts.
    pub fn build_context(&self) -> Result<String> {
        let mut ctx = String::new();

        ctx.push_str(&self.soul.build_context());
        ctx.push('\n');
        ctx.push_str(&self.identity.build_context());
        ctx.push('\n');

        let memory_ctx = self.memory.build_context()?;
        if !memory_ctx.is_empty() {
            ctx.push_str(&memory_ctx);
            ctx.push('\n');
        }

        let skill_ctx = self.skills.build_context()?;
        if !skill_ctx.is_empty() {
            ctx.push_str(&skill_ctx);
        }

        Ok(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> opencrab_db::Db {
        let conn = opencrab_db::init_memory().unwrap();
        opencrab_db::Db::from_connection(conn)
    }

    #[test]
    fn test_agent_new() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = test_conn();
        let soul = Soul::new("TestPersona");
        let identity = Identity::new("agent-1", "TestAgent");
        let agent = Agent::new(
            "agent-1",
            soul,
            identity,
            conn,
            dir.path(),
            AgentLlmConfig::default(),
            HeartbeatConfig::default(),
        )
        .unwrap();

        assert_eq!(agent.id, "agent-1");
        assert_eq!(agent.soul.persona_name, "TestPersona");
        assert_eq!(agent.identity.name, "TestAgent");
    }

    #[test]
    fn test_agent_load_from_db() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = test_conn();

        {
            let db = conn.lock().unwrap();
            queries::upsert_agent(
                &db,
                &queries::AgentRow {
                    agent_id: "agent-1".to_string(),
                    name: "LoadedAgent".to_string(),
                    job_title: None,
                    organization: None,
                    image_url: None,
                    persona_name: "LoadedPersona".to_string(),
                    personality: None,
                    instructions: String::new(),
                    heartbeat_instructions: String::new(),
                    model: None,
                    reasoning_effort: None,
                    web_search: None,
                    metadata_json: None,
                },
            )
            .unwrap();
        }

        let agent = Agent::load(
            "agent-1",
            conn,
            dir.path(),
            AgentLlmConfig::default(),
            HeartbeatConfig::default(),
        )
        .unwrap();

        assert_eq!(agent.identity.name, "LoadedAgent");
        assert_eq!(agent.soul.persona_name, "LoadedPersona");
    }

    #[test]
    fn test_agent_build_context() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = test_conn();
        let soul = Soul::new("TestPersona");
        let identity = Identity::new("agent-1", "TestAgent");
        let agent = Agent::new(
            "agent-1",
            soul,
            identity,
            conn,
            dir.path(),
            AgentLlmConfig::default(),
            HeartbeatConfig::default(),
        )
        .unwrap();

        let ctx = agent.build_context().unwrap();
        assert!(ctx.contains("TestPersona"));
        assert!(ctx.contains("TestAgent"));
    }
}
