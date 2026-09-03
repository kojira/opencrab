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

pub mod a2ui;
pub mod agent;
pub mod caller;
pub mod context_budget;
pub mod continue_marker;
pub mod conversation;
pub mod conversation_typed;
pub mod engine;
pub mod evaluator;
pub mod heartbeat;
pub mod identity;
pub mod import;
pub mod impression_section;
pub mod injection;
pub mod llm_text;
pub mod memory;
pub mod memory_index;
pub mod owner;
pub mod runtime_context;
pub mod secret_box;
pub mod skill;
pub mod soul;
pub mod task_ledger;
pub mod text_delivery;
pub mod tokens;
pub mod tool_result_log;
pub mod workspace;

// Re-export primary types for convenience.
pub use agent::{Agent, AgentLlmConfig, AgentModels, ModelRef};
pub use caller::CallerIdentity;
pub use continue_marker::{
    strip_trailing_continue, terminate_at_no_reply, NoReplyTermination, CONTINUE_LOG_TARGET,
    CONTINUE_SENTINEL, NO_REPLY_SENTINEL,
};
pub use engine::{
    ActionExecutor, ActionResult, ChatRequest, ChatResponse, DispatchCall, DispatchOutcome,
    EngineResult, FoldedInbound, FunctionDefinition, LiveInboundSource, LlmCallLog, LlmClient,
    SkillEngine, ToolCall, ToolDispatcher,
};
pub use heartbeat::HeartbeatConfig;
pub use identity::Identity;
pub use memory::MemoryManager;
pub use owner::{is_owner_id, owner_is_unset};
pub use runtime_context::prepend_runtime_context;
pub use skill::{Skill, SkillManager, SkillSource};
pub use soul::{Soul, ThinkingStyle};
pub use workspace::{FileEntry, Workspace};

// A2UI types
pub use a2ui::{
    build_confirmation_components, A2uiAction, A2uiComponent, A2uiComponentType, A2uiSurface,
    A2uiUserAction, PendingInteraction, PendingInteractionRegistry, PendingUiSurface, RenderError,
    RenderTarget, RenderedMessage, UiRenderer, UiResponseEvent, UiResponseSink, UserActionResponse,
};
