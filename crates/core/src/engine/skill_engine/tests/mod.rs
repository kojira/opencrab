use super::*;
use async_trait::async_trait;
use opencrab_llm_types::{
    ChatResponse, Choice, FunctionCall, FunctionDefinition, MessageContent, Usage,
};
use serde_json::Value;

static TRACING_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

include!("support.rs");
include!("budget.rs");
include!("request_and_callbacks.rs");
include!("utterance.rs");
include!("continuation.rs");
include!("tool_results_dispatch.rs");
include!("dispatch_ordering.rs");
include!("live_tool_completion_requests.rs");
