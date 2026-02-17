use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

use crate::heartbeat::HeartbeatConfig;
use crate::identity::{AgentRole, Identity};
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
        conn: Arc<Mutex<Connection>>,
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
        conn: Arc<Mutex<Connection>>,
        workspace_root: impl Into<std::path::PathBuf>,
        llm_config: AgentLlmConfig,
        heartbeat: HeartbeatConfig,
    ) -> Result<Self> {
        let (soul, identity) = {
            let db = conn.lock().unwrap();

            let soul_row = queries::get_soul(&db, agent_id)?
                .with_context(|| format!("Soul not found for agent: {}", agent_id))?;

            let identity_row = queries::get_identity(&db, agent_id)?
                .with_context(|| format!("Identity not found for agent: {}", agent_id))?;

            let soul = Soul {
                persona_name: soul_row.persona_name,
                social_style: serde_json::from_str(&soul_row.social_style_json)
                    .unwrap_or_default(),
                personality: serde_json::from_str(&soul_row.personality_json)
                    .unwrap_or_default(),
                thinking_style: serde_json::from_str(&soul_row.thinking_style_json)
                    .unwrap_or_default(),
                custom_traits: soul_row
                    .custom_traits_json
                    .and_then(|s| serde_json::from_str(&s).ok()),
            };

            let identity = Identity {
                agent_id: identity_row.agent_id,
                name: identity_row.name,
                role: AgentRole::from_str_value(&identity_row.role),
                job_title: identity_row.job_title,
                organization: identity_row.organization,
                image_url: identity_row.image_url,
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
