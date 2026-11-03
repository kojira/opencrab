use opencrab_llm_types::Message;

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
    // D-1111: appended at the end in arrival order (the one turn-view writer).
    super::turn_view::inbound(messages, ledger, &text);
    if let Some(origin) = origin {
        if !silent_origins.was_read(&origin) && !pending_read_origins.contains(&origin) {
            pending_read_origins.push(origin);
        }
    }
}
