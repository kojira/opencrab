use std::collections::{HashMap, HashSet};

use opencrab_llm_types::{ChatRequest, FunctionDefinition, Message, MessageContent, Role};
use sha2::{Digest, Sha256};

use super::SkillEngine;
use crate::context_budget::TokenLedger;

pub(super) fn append_live_inbound(
    engine: &SkillEngine,
    messages: &mut Vec<Message>,
    turn_ledger: &mut TokenLedger,
    pending_origins: &mut Vec<String>,
    emitted_origins: &HashSet<String>,
) {
    let Some(source) = &engine.live_inbound else {
        return;
    };
    for folded in source.poll_new_with_origin() {
        let crate::FoldedInbound { text, origin } = folded;
        tracing::info!(
            bytes = text.len(),
            "injecting newly arrived user speech into the running turn"
        );
        messages.push(Message {
            role: Role::User,
            content: Some(MessageContent::Text(text.clone())),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        });
        turn_ledger.record(format!("live:{}", messages.len()), &text);
        if let Some(origin) = origin {
            if !emitted_origins.contains(&origin) && !pending_origins.contains(&origin) {
                pending_origins.push(origin);
            }
        }
    }
}

pub(super) fn turn_state_digest(messages: &[Message]) -> anyhow::Result<[u8; 32]> {
    let encoded = serde_json::to_vec(messages)?;
    Ok(Sha256::digest(encoded).into())
}

pub(super) fn build_chat_request(
    engine: &SkillEngine,
    model: String,
    messages: &[Message],
    tools: Vec<FunctionDefinition>,
) -> anyhow::Result<ChatRequest> {
    let max_tokens = match engine.resolved_model_input_limits(&model)? {
        Some(limits) => Some(
            limits
                .max_output_tokens
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    anyhow::anyhow!("model max_output_tokens is missing or non-positive")
                })?
                .try_into()?,
        ),
        None => engine.max_output_tokens,
    };
    let mut metadata: HashMap<String, serde_json::Value> = HashMap::new();
    if engine.web_search {
        metadata.insert("web_search".to_string(), serde_json::json!(true));
    }
    Ok(ChatRequest {
        model,
        messages: messages.to_vec(),
        functions: (!tools.is_empty()).then_some(tools),
        function_call: None,
        temperature: Some(0.7),
        max_tokens,
        stop: None,
        stream: None,
        metadata,
        agent_id: None,
        reasoning_effort: engine.reasoning_effort.clone(),
    })
}
