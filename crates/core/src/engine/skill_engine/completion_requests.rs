use std::collections::HashSet;

use anyhow::Result;
use opencrab_llm_types::{ChatRequest, Message, MessageContent, Role, ToolCall};
use sha2::{Digest, Sha256};

use super::SkillEngine;
use crate::context_budget::TokenLedger;
use crate::context_budget::{
    allocate_result_tokens, effective_input_limit, page_utf8_result, validate_final_request_tokens,
};
use crate::LiveToolCompletionSource;

/// durable response replayで永続済みのtool effectを会話用本文へ戻す。
pub(super) fn recover_tool_effect(
    source: Option<&dyn LiveToolCompletionSource>,
    replayed: bool,
    call: &ToolCall,
) -> Option<(String, bool)> {
    let effect = replayed.then(|| source?.recover_tool_effect(&call.id))??;
    let running = effect.lifecycle_status == "running";
    let body = if running {
        format!("[<{}] status:running tool:{}", call.id, call.function.name)
    } else {
        effect.content
    };
    Some((body, running))
}

pub(super) fn recover_dispatch_effects(
    source: Option<&dyn LiveToolCompletionSource>,
    replayed: bool,
    calls: &[ToolCall],
) -> Option<Vec<(String, bool)>> {
    replayed.then(|| {
        calls
            .iter()
            .map(|call| recover_tool_effect(source, true, call))
            .collect()
    })?
}

pub(super) fn assign_conversation_tool_ids(
    engine: &SkillEngine,
    calls: &mut [ToolCall],
    replayed: bool,
) -> Result<()> {
    if !engine.short_tool_ids_enabled {
        return Ok(());
    }
    for call in calls {
        let provider_id = call.id.clone();
        if replayed {
            call.id = engine
                .live_tool_completions
                .as_ref()
                .and_then(|source| source.resolve_conversation_tool_id(&provider_id))
                .ok_or_else(|| {
                    anyhow::anyhow!("missing persisted tool ID correlation for replayed call")
                })?;
            continue;
        }
        let sequence = engine
            .next_tool_sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let short_id = format!("t{sequence}");
        call.id = short_id.clone();
        for callback in &engine.on_tool_id_correlation {
            callback(short_id.clone(), provider_id.clone(), sequence);
        }
    }
    Ok(())
}

/// 永続completion eventを一度だけturn内messagesへ追加し、参照版の位置も記録する。
pub(super) fn append_live_completions(
    engine: &SkillEngine,
    messages: &mut Vec<Message>,
    turn_ledger: &mut TokenLedger,
    folded_ids: &mut HashSet<String>,
    pending_event_ids: &mut Vec<String>,
    alternates: &mut Vec<(usize, String)>,
    running_background_batches: &mut usize,
) {
    let Some(source) = &engine.live_tool_completions else {
        return;
    };
    for completion in source.poll_tool_completions() {
        if !folded_ids.insert(completion.event_id.clone()) {
            continue;
        }
        pending_event_ids.push(completion.event_id.clone());
        *running_background_batches = running_background_batches.saturating_sub(1);
        let already_in_history = messages.iter().any(|message| {
            matches!(&message.content, Some(MessageContent::Text(text)) if text.contains(&completion.text))
        });
        if already_in_history {
            continue;
        }
        messages.push(Message {
            role: Role::User,
            content: Some(MessageContent::Text(completion.text.clone())),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        });
        alternates.push((messages.len() - 1, completion.omitted_text));
        turn_ledger.record(
            format!("tool_completion:{}", completion.event_id),
            &completion.text,
        );
    }
}

pub(super) fn register_tool_result_alternate(
    messages: &[Message],
    alternates: &mut Vec<(usize, String)>,
    tool_call_id: &str,
    tool_name: &str,
    result: &str,
) {
    let status = serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|value| value.get("success").and_then(|v| v.as_bool()))
        .map_or(
            "completed",
            |success| if success { "completed" } else { "failed" },
        );
    let path = result
        .split(" to `")
        .nth(1)
        .and_then(|rest| rest.split('`').next());
    let omitted = serde_json::json!({
        "status": status,
        "tool": tool_name,
        "conversation_tool_id": tool_call_id,
        "result_omitted": true,
        "result_path": path,
        "result_bytes": result.len(),
        "result_lines": result.lines().count(),
    })
    .to_string();
    alternates.push((messages.len() - 1, omitted));
}

/// 実providerのexact/certified meterだけを使い、上限超過またはmeter不在なら本文を参照化する。
pub(super) fn prepare_completion_request(
    engine: &SkillEngine,
    mut request: ChatRequest,
    turn_messages: &mut [Message],
    alternates: &[(usize, String)],
    event_ids: &[String],
) -> Result<(ChatRequest, Option<String>)> {
    let recovered = engine
        .live_tool_completions
        .as_ref()
        .map(|source| source.recover_included_request(event_ids))
        .transpose()
        .map_err(anyhow::Error::msg)?
        .flatten();
    if let Some((request_id, exact_request)) = recovered {
        validate_exact_completion_request(engine, &exact_request)?;
        return Ok((exact_request, Some(request_id)));
    }
    apply_completion_input_budget(engine, &mut request, turn_messages, alternates)?;
    let request_id = mark_completion_request_included(engine, event_ids, &request)?;
    Ok((request, request_id))
}

fn mark_completion_request_included(
    engine: &SkillEngine,
    event_ids: &[String],
    request: &ChatRequest,
) -> Result<Option<String>> {
    if event_ids.is_empty() {
        return Ok(None);
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    let encoded = serde_json::to_vec(request)?;
    let digest = format!("{:x}", Sha256::digest(&encoded));
    let request_json = String::from_utf8(encoded)?;
    if let Some(source) = &engine.live_tool_completions {
        source
            .mark_included(event_ids, &request_id, &digest, &request_json)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(Some(request_id))
}

pub(super) fn mark_completion_effect_applied(
    engine: &SkillEngine,
    request_id: Option<&str>,
) -> Result<()> {
    if let (Some(source), Some(request_id)) = (&engine.live_tool_completions, request_id) {
        source
            .mark_effect_applied(request_id)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(())
}

pub(super) fn mark_completion_request_consumed(
    engine: &SkillEngine,
    event_ids: &mut Vec<String>,
    request_id: Option<&str>,
) -> Result<()> {
    if let (Some(source), Some(request_id)) = (&engine.live_tool_completions, request_id) {
        source
            .mark_consumed(event_ids, request_id)
            .map_err(anyhow::Error::msg)?;
        event_ids.clear();
    }
    Ok(())
}

fn validate_exact_completion_request(engine: &SkillEngine, request: &ChatRequest) -> Result<()> {
    let limits = engine
        .resolved_model_input_limits(&request.model)?
        .ok_or_else(|| anyhow::anyhow!("model input limits unavailable for completion request"))?;
    let effective_limit =
        effective_input_limit(limits, request.max_tokens.map(|value| value as usize))?;
    let measured = engine
        .llm
        .measure_request_tokens(request)
        .ok_or_else(|| anyhow::anyhow!("certified request token meter unavailable"))?;
    validate_final_request_tokens(measured.tokens, effective_limit)?;
    Ok(())
}

pub(super) fn apply_completion_input_budget(
    engine: &SkillEngine,
    request: &mut ChatRequest,
    turn_messages: &mut [Message],
    alternates: &[(usize, String)],
) -> Result<()> {
    let has_tool_result = request
        .messages
        .iter()
        .any(|message| message.role == Role::Tool);
    if alternates.is_empty() && !has_tool_result {
        return Ok(());
    }
    let Some(limits) = engine.resolved_model_input_limits(&request.model)? else {
        if alternates.is_empty() {
            // Productionはstartup validationで上限を必須化済み。resolverを持たない
            // unit harnessだけは従来のtool-loop契約を維持する。
            return Ok(());
        }
        anyhow::bail!("model input limits unavailable for completion request");
    };
    let effective_limit =
        effective_input_limit(limits, request.max_tokens.map(|value| value as usize))?;
    let must_omit = engine
        .llm
        .measure_request_tokens(request)
        .map(|measured| measured.tokens > effective_limit)
        .unwrap_or(true);
    if must_omit {
        let originals = alternates
            .iter()
            .map(|(index, _)| {
                request
                    .messages
                    .get(*index)
                    .and_then(|message| message.content.as_ref())
                    .and_then(|content| match content {
                        MessageContent::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or_default()
                    .to_string()
            })
            .collect::<Vec<_>>();
        for (index, omitted) in alternates {
            if let Some(message) = request.messages.get_mut(*index) {
                message.content = Some(MessageContent::Text(omitted.clone()));
            }
        }
        let base = engine
            .llm
            .measure_request_tokens(request)
            .ok_or_else(|| anyhow::anyhow!("certified request token meter unavailable"))?;
        validate_final_request_tokens(base.tokens, effective_limit)?;
        let available = effective_limit.saturating_sub(base.tokens);
        let required = originals.iter().map(String::len).collect::<Vec<_>>();
        let allocated = allocate_result_tokens(available, &required);
        for (((index, omitted), original), allocation) in
            alternates.iter().zip(&originals).zip(allocated)
        {
            let prefix_cap = allocation.saturating_sub(1);
            if prefix_cap == 0 {
                continue;
            }
            let page = page_utf8_result(original, 0, prefix_cap)?;
            if let Some(message) = request.messages.get_mut(*index) {
                message.content = Some(MessageContent::Text(format!("{}\n{}", page.body, omitted)));
            }
        }
    }
    let measured = engine
        .llm
        .measure_request_tokens(request)
        .ok_or_else(|| anyhow::anyhow!("certified request token meter unavailable"))?;
    validate_final_request_tokens(measured.tokens, effective_limit)?;
    for (index, _) in alternates {
        if let (Some(source), Some(target)) =
            (request.messages.get(*index), turn_messages.get_mut(*index))
        {
            target.content = source.content.clone();
        }
    }
    Ok(())
}
