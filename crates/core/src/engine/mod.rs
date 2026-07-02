//! LLM-driven agent engine.

pub mod skill_engine;
pub mod types;
pub mod xml_parser;

pub use skill_engine::SkillEngine;
pub use types::{
    ActionExecutor, ActionResult, ChatRequest, ChatResponse, EngineResult, FunctionDefinition,
    LlmCallLog, LlmClient, ToolCall,
};
pub use xml_parser::parse_xml_tool_calls;
