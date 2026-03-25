use serde::{Deserialize, Serialize};

/// The identity of an agent: who they are in the world.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// Unique identifier for this agent.
    pub agent_id: String,
    /// Display name.
    pub name: String,
    /// Professional title (e.g., "Senior Engineer").
    pub job_title: Option<String>,
    /// Organization affiliation.
    pub organization: Option<String>,
    /// URL to the agent's avatar or profile image.
    pub image_url: Option<String>,
}

impl Identity {
    /// Create a new Identity with the given agent_id and name.
    pub fn new(agent_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            name: name.into(),
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

        if let Some(ref title) = self.job_title {
            ctx.push_str(&format!("- Job Title: {}\n", title));
        }

        if let Some(ref org) = self.organization {
            ctx.push_str(&format!("- Organization: {}\n", org));
        }

        ctx
    }
}
