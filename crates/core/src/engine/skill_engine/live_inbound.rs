use opencrab_llm_types::{Message, MessageContent, Role};

use super::silent_origins::SilentOriginTracker;
use crate::context_budget::TokenLedger;
use crate::FoldedInbound;

/// Folded inbound本文を次requestへ追加し、opaque originをread待ちへ一度だけ積む。
pub(super) fn append(
    folded: FoldedInbound,
    messages: &mut Vec<Message>,
    ledger: &mut TokenLedger,
    pending_read_origins: &mut Vec<String>,
    silent_origins: &SilentOriginTracker,
    iteration: Option<usize>,
) {
    let FoldedInbound { text, origin } = folded;
    if let Some(iteration) = iteration {
        tracing::info!(
            iteration,
            bytes = text.len(),
            "injecting newly arrived user speech into the running turn"
        );
    }
    messages.push(Message {
        role: Role::User,
        content: Some(MessageContent::Text(text.clone())),
        name: None,
        function_call: None,
        tool_calls: None,
        tool_call_id: None,
    });
    ledger.record(format!("live:{}", messages.len()), &text);
    if let Some(origin) = origin {
        if !silent_origins.was_read(&origin) && !pending_read_origins.contains(&origin) {
            pending_read_origins.push(origin);
        }
    }
}
