//! D-1083: release complete sentences of an LLM call's output text while the call is running.
//!
//! Only engines with an [`PartialTextHook`] use the observed LLM call. The final exchange is
//! processed exactly as on the unobserved path; partial text is a best-effort preview.

use std::sync::Arc;

use anyhow::Result;

use super::SkillEngine;
use crate::continue_marker::NO_REPLY_SENTINEL;
use crate::engine::types::{ChatRequest, LlmExchange, OutputTextEvent, OutputTextSink};

/// Event delivered to [`PartialTextHook`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartialTextEvent {
    /// A new stream starts: at the start of every LLM call and after every provider retry.
    Begin { stream_id: String },
    /// The cumulative released text of the stream. Sent only when it grows.
    Text { stream_id: String, text: String },
}

/// Fire-and-forget receiver of [`PartialTextEvent`]s. The engine awaits it; it cannot fail the
/// turn.
pub type PartialTextHook = Arc<
    dyn Fn(PartialTextEvent) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

const SENTENCE_TERMINATORS: [char; 6] = ['\n', '。', '！', '？', '!', '?'];
const TOOL_MARKUP_START: &str = "<function_calls";

/// Pure state machine that decides which prefix of the streamed text may be released.
#[derive(Debug, Default)]
pub struct PartialTextGate {
    buffer: String,
    released_len: usize,
    frozen: bool,
}

impl PartialTextGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a delta and returns the cumulative released text when it grew.
    pub fn push(&mut self, delta: &str) -> Option<String> {
        self.buffer.push_str(delta);
        if self.frozen {
            return None;
        }
        // R2: a buffer that may still become the bare NO_REPLY marker releases nothing.
        let trimmed = self.buffer.trim();
        if trimmed.is_empty() || NO_REPLY_SENTINEL.starts_with(trimmed) {
            return None;
        }
        // R3: text from a tool markup start on is never released, now or later. A `<` that may
        // still grow into one holds the text after it until it is decided.
        let mut limit = self.buffer.len();
        if let Some((markup_start, complete)) = self.possible_markup_start() {
            self.frozen = complete;
            limit = markup_start;
        }
        let cut = self.release_cut(limit)?;
        if cut <= self.released_len {
            return None;
        }
        self.released_len = cut;
        Some(self.buffer[..cut].to_string())
    }

    /// First unreleased `<` that starts tool markup (`true`) or may still grow into it (`false`).
    fn possible_markup_start(&self) -> Option<(usize, bool)> {
        self.buffer[self.released_len..]
            .match_indices('<')
            .map(|(offset, _)| self.released_len + offset)
            .find_map(|start| {
                let rest = &self.buffer[start..];
                if rest.starts_with(TOOL_MARKUP_START) {
                    Some((start, true))
                } else if TOOL_MARKUP_START.starts_with(rest) {
                    Some((start, false))
                } else {
                    None
                }
            })
    }

    /// R1 + R2b: end of the last terminator within `limit`, never ending on a NO_REPLY marker
    /// that only whitespace follows.
    fn release_cut(&self, limit: usize) -> Option<usize> {
        let mut limit = limit;
        loop {
            let cut = last_terminator_end(&self.buffer[..limit])?;
            match no_reply_marker_start(&self.buffer[..cut]) {
                Some(marker_start) if self.buffer[cut..].trim().is_empty() => limit = marker_start,
                _ => return Some(cut),
            }
        }
    }
}

fn last_terminator_end(text: &str) -> Option<usize> {
    text.char_indices()
        .rev()
        .find(|(_, c)| SENTENCE_TERMINATORS.contains(c))
        .map(|(i, c)| i + c.len_utf8())
}

/// Start of a trailing NO_REPLY marker in `text`, by the same rule as `terminate_at_no_reply`.
fn no_reply_marker_start(text: &str) -> Option<usize> {
    let before = text.trim_end().strip_suffix(NO_REPLY_SENTINEL)?;
    if before
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    Some(before.len())
}

/// One stream of one LLM call: its id, gate, and the hook that receives its events.
struct PartialTextStream {
    hook: PartialTextHook,
    stream_id: String,
    gate: PartialTextGate,
}

impl PartialTextStream {
    async fn begin(hook: PartialTextHook) -> Self {
        let stream_id = uuid::Uuid::new_v4().to_string();
        hook(PartialTextEvent::Begin {
            stream_id: stream_id.clone(),
        })
        .await;
        Self {
            hook,
            stream_id,
            gate: PartialTextGate::new(),
        }
    }

    async fn apply(&mut self, event: OutputTextEvent) {
        match event {
            OutputTextEvent::Delta(delta) => {
                if let Some(text) = self.gate.push(&delta) {
                    (self.hook)(PartialTextEvent::Text {
                        stream_id: self.stream_id.clone(),
                        text,
                    })
                    .await;
                }
            }
            OutputTextEvent::Reset => {
                *self = Self::begin(Arc::clone(&self.hook)).await;
            }
        }
    }
}

impl SkillEngine {
    /// One LLM call of the run loop. With [`Self::on_partial_text`] set, the call is observed and
    /// every partial event is delivered before this returns, so before any say of this call.
    pub(super) async fn chat_for_iteration(&self, request: ChatRequest) -> Result<LlmExchange> {
        let Some(hook) = self.on_partial_text.clone() else {
            return self.llm.chat_with_history(request).await;
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink: OutputTextSink = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut stream = PartialTextStream::begin(hook).await;
        let call = self.llm.chat_with_history_observed(request, sink);
        tokio::pin!(call);
        let result = loop {
            tokio::select! {
                result = &mut call => break result,
                Some(event) = rx.recv() => stream.apply(event).await,
            }
        };
        while let Ok(event) = rx.try_recv() {
            stream.apply(event).await;
        }
        result
    }
}

#[cfg(test)]
#[path = "partial_text_tests.rs"]
mod tests;
