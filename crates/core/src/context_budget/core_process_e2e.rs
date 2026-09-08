//! 826-B 必須 4 点（mock LLM のみ）。SkillEngine の MockLlm 基盤と本番組立/終了経路で回す。

use super::compact::{
    compact_to_low_water, should_compact, CompactItem, CompactLane, CompactPhase,
};
use super::governor::{
    assemble_from_snapshot, items_from_logs, take_governor_events, GovernorEvent, TurnGovernor,
};
use crate::conversation::build_conversation_string_with_waters;
use crate::engine::{ActionExecutor, ActionResult, ChatRequest, SkillEngine};
use crate::tokens::estimate_tokens;
use crate::LlmClient;
use async_trait::async_trait;
use opencrab_llm_types::{
    ChatResponse, Choice, FunctionDefinition, Message, MessageContent, Role, Usage,
};

include!("core_process_e2e/governor_scenarios.rs");
include!("core_process_e2e/assembly_identifiers.rs");
include!("core_process_e2e/typed_walkthrough.rs");
