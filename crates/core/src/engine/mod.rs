//! LLM-driven agent engine.

pub mod skill_engine;
pub mod types;
pub mod xml_parser;

pub use skill_engine::SkillEngine;
pub use types::{
    ActionExecutor, ActionResult, ChatRequest, ChatResponse, DispatchCall, DispatchOutcome,
    EngineResult, FoldedInbound, FoldedToolCompletion, FunctionDefinition, LiveInboundSource,
    LiveToolCompletionSource, LlmCallLog, LlmClient, LlmExchange, LlmExchangeLog,
    ProviderToolHistory, RecoveredToolEffect, RequestTokenMeasurement, RequestTokenMeterCapability,
    ToolCall, ToolDispatcher,
};
pub use xml_parser::parse_xml_tool_calls;
