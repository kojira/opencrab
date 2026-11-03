//! D-1111: the single writer of a running turn's request view.
//!
//! Every event of the turn (delivered speech, tool call, tool result, newly arrived inbound)
//! is appended at the END of the message list, in the order it happened. Nothing is inserted
//! into the middle. If the last message is the user message carrying `<conversation_history>`,
//! a visible event (speech, inbound) joins that history's tail — which is the same position.
//! Otherwise a visible event becomes a new user message, rendered exactly as it appears in
//! `<conversation_history>` on later turns.

use anyhow::Result;
use opencrab_llm_types::{Message, MessageContent, Role, ToolCall};

use super::silent_origins::SilentOriginTracker;
use super::turn_budget::{
    append_bounded_history_block, append_or_create_bounded_history_block, message_plain_text,
};
use crate::context_budget::TokenLedger;
use crate::conversation::{CONVERSATION_HISTORY_END, CONVERSATION_HISTORY_START};

/// Appends a visible entry (already rendered in the conversation form) at the end.
fn append_visible(
    messages: &mut Vec<Message>,
    ledger: &mut TokenLedger,
    rendered: &str,
    key: &str,
    may_create_history: bool,
) -> Result<()> {
    let joined = if last_is_history(messages) {
        append_bounded_history_block(messages, ledger, rendered)
    } else if may_create_history && last_is_first_user_without_history(messages) {
        append_or_create_bounded_history_block(messages, ledger, rendered)?
    } else {
        false
    };
    if !joined {
        messages.push(Message {
            role: Role::User,
            content: Some(MessageContent::Text(rendered.to_string())),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        });
        ledger.record(format!("{key}:{}", messages.len()), rendered);
    }
    Ok(())
}

fn last_is_history(messages: &[Message]) -> bool {
    messages.last().is_some_and(|m| {
        m.role == Role::User && message_plain_text(m).contains(CONVERSATION_HISTORY_END)
    })
}

fn last_is_first_user_without_history(messages: &[Message]) -> bool {
    let first_user = messages.iter().position(|m| m.role == Role::User);
    first_user.is_some()
        && first_user == messages.len().checked_sub(1)
        && !messages
            .iter()
            .any(|m| message_plain_text(m).contains(CONVERSATION_HISTORY_START))
}

/// The agent's speech is refused when the request's history boundary is malformed or doubled
/// (as before D-1111), rather than guessing where it belongs.
fn check_boundaries(messages: &[Message]) -> Result<()> {
    let count = |marker: &str| {
        messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| message_plain_text(m).matches(marker).count())
            .sum::<usize>()
    };
    let (starts, ends) = (
        count(CONVERSATION_HISTORY_START),
        count(CONVERSATION_HISTORY_END),
    );
    if (starts, ends) != (0, 0) && (starts, ends) != (1, 1) {
        anyhow::bail!(
            "malformed or multiple <conversation_history> boundaries: found {starts} opening and {ends} closing markers"
        );
    }
    Ok(())
}

/// The agent's delivered speech. `name` is the agent's display name in the history; a run
/// without one (unit runs) keeps the speech as a plain assistant message at the end.
pub(super) fn speech_entry(
    messages: &mut Vec<Message>,
    ledger: &mut TokenLedger,
    name: Option<&str>,
    text: &str,
) -> Result<()> {
    let speech = text;
    let Some(name) = name else {
        messages.push(Message {
            role: Role::Assistant,
            content: Some(MessageContent::Text(speech.to_string())),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        });
        ledger.record(format!("asst:{}", messages.len()), speech);
        return Ok(());
    };
    check_boundaries(messages)?;
    let created_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let rendered = crate::conversation::format_speech_entry(name, Some(&created_at), speech);
    append_visible(messages, ledger, &rendered, "speech", true)
}

/// Newly arrived inbound speech, already rendered by the source.
pub(super) fn inbound(messages: &mut Vec<Message>, ledger: &mut TokenLedger, rendered: &str) {
    // Joining an existing history cannot fail on its own boundary check (it is the last message).
    let _ = append_visible(messages, ledger, rendered, "live", false);
}

/// The assistant's tool calls. Any text of the same generation is passed here only when it was
/// not delivered (a delivered one is appended as [`speech`] first).
pub(super) fn tool_calls(
    messages: &mut Vec<Message>,
    ledger: &mut TokenLedger,
    undelivered_text: Option<String>,
    calls: Vec<ToolCall>,
) {
    let text = undelivered_text.unwrap_or_default();
    messages.push(Message {
        role: Role::Assistant,
        content: (!text.is_empty()).then(|| MessageContent::Text(text.clone())),
        name: None,
        function_call: None,
        tool_calls: Some(calls),
        tool_call_id: None,
    });
    ledger.record(format!("asst:{}", messages.len()), &text);
}

/// One tool result (or acknowledgement) for `tool_call_id`.
pub(super) fn tool_result(
    messages: &mut Vec<Message>,
    ledger: &mut TokenLedger,
    tool_call_id: &str,
    result_json: &str,
) {
    messages.push(Message::tool(
        tool_call_id.to_string(),
        result_json.to_string(),
    ));
    ledger.record(format!("tool:{}", messages.len()), result_json);
}

/// The turn's request view, its ledger and its silent-origin tracker.
pub(super) type TurnState<'a> = (
    &'a mut Vec<Message>,
    &'a mut TokenLedger,
    &'a mut SilentOriginTracker,
);

impl super::SkillEngine {
    /// D-1111: the one place a speech is delivered during a turn. The delivery hook sends and
    /// records it (one history row), and the same call appends it at the end of the turn's
    /// request view. Returns false when the run has no delivery hook (nothing happened).
    pub(super) async fn deliver_speech(
        &self,
        what: &str,
        speech: &str,
        (messages, ledger, silent_origins): TurnState<'_>,
    ) -> Result<bool> {
        let Some(cb) = &self.on_continuation_speech else {
            return Ok(false);
        };
        cb(speech.to_string())
            .await
            .map_err(|e| anyhow::anyhow!("{what} delivery failed: {e:#}"))?;
        silent_origins.resolve_visible();
        speech_entry(
            messages,
            ledger,
            self.assistant_history_name.as_deref(),
            speech,
        )?;
        Ok(true)
    }

    /// A text-only speech that continues the turn. It is delivered now through the hook, or —
    /// on a run without one — returned to be delivered as the final reply. Either way the model
    /// sees it as said, at the end of the view.
    pub(super) async fn speech_or_hold(
        &self,
        speech: &str,
        (messages, ledger, silent_origins): TurnState<'_>,
    ) -> Result<Option<String>> {
        if self
            .deliver_speech(
                "continuation speech",
                speech,
                (messages, ledger, silent_origins),
            )
            .await?
        {
            return Ok(None);
        }
        silent_origins.resolve_visible();
        speech_entry(
            messages,
            ledger,
            self.assistant_history_name.as_deref(),
            speech,
        )?;
        Ok(Some(speech.to_string()))
    }
}
