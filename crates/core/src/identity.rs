use serde::{Deserialize, Serialize};
use std::fmt;

/// The role an agent plays within a session or group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// A regular participant who contributes to discussions.
    Discussant,
    /// Guides and manages the flow of conversation.
    Facilitator,
    /// Watches and analyzes without actively participating.
    Observer,
    /// A custom role defined by the user.
    Custom(String),
}

impl fmt::Display for AgentRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentRole::Discussant => write!(f, "discussant"),
            AgentRole::Facilitator => write!(f, "facilitator"),
            AgentRole::Observer => write!(f, "observer"),
            AgentRole::Custom(role) => write!(f, "{}", role),
        }
    }
}

impl AgentRole {
    /// Parse a role from a string value.
    pub fn from_str_value(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "discussant" => AgentRole::Discussant,
            "facilitator" => AgentRole::Facilitator,
            "observer" => AgentRole::Observer,
            other => AgentRole::Custom(other.to_string()),
        }
    }
}

/// The identity of an agent: who they are in the world.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// Unique identifier for this agent.
    pub agent_id: String,
    /// Display name.
    pub name: String,
    /// Role in sessions.
    pub role: AgentRole,
    /// Professional title (e.g., "Senior Engineer").
    pub job_title: Option<String>,
    /// Organization affiliation.
    pub organization: Option<String>,
    /// URL to the agent's avatar or profile image.
    pub image_url: Option<String>,
}

impl Identity {
    /// Create a new Identity with the given agent_id, name, and role.
    pub fn new(
        agent_id: impl Into<String>,
        name: impl Into<String>,
        role: AgentRole,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            name: name.into(),
            role,
            job_title: None,
            organization: None,
            image_url: None,
        }
    }

    /// Build a context string describing this identity for LLM prompts.
    pub fn build_context(&self) -> String {
        let mut ctx = String::new();

        ctx.push_str(&format!("## Identity\n\n"));
        ctx.push_str(&format!("- Name: {}\n", self.name));
        ctx.push_str(&format!("- Role: {}\n", self.role));

        if let Some(ref title) = self.job_title {
            ctx.push_str(&format!("- Job Title: {}\n", title));
        }

        if let Some(ref org) = self.organization {
            ctx.push_str(&format!("- Organization: {}\n", org));
        }

        ctx
    }
}
