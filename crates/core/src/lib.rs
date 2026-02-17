//! OpenCrab Core - Agent engine types and components.
//!
//! This crate contains the core types that make up an AI agent in the
//! OpenCrab framework:
//!
//! - **Soul**: Personality traits, social style, and thinking preferences.
//! - **Identity**: Name, role, and organizational context.
//! - **Memory**: Curated memories and session log management.
//! - **Skill**: Standard and acquired skill management.
//! - **Workspace**: Sandboxed file operations with path traversal protection.
//! - **Heartbeat**: Periodic agent activity loop.
//! - **Agent**: The combined agent struct.
//! - **Engine**: LLM-driven action loop for executing skills.

pub mod soul;
pub mod identity;
pub mod memory;
pub mod skill;
pub mod workspace;
pub mod heartbeat;
pub mod agent;
pub mod engine;

// Re-export primary types for convenience.
pub use soul::{Soul, SocialStyle, Personality, ThinkingStyle};
pub use identity::{Identity, AgentRole};
pub use memory::MemoryManager;
pub use skill::{SkillManager, Skill, SkillSource};
pub use workspace::{Workspace, FileEntry};
pub use heartbeat::{HeartbeatConfig, HeartbeatDecision};
pub use agent::{Agent, AgentLlmConfig, AgentModels, ModelRef};
pub use engine::{
    SkillEngine, ActionExecutor, ActionResult, LlmClient,
    ChatRequestSimple, ChatResponseSimple, ChatMessage,
    ToolDefinition, ToolCall, UsageInfo, EngineResult,
};
