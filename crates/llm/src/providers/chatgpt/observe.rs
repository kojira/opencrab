//! D-1083: incremental read of a 2xx Responses SSE body that forwards output text deltas.
//!
//! The body is still accumulated in full and parsed by `parse_exchange`, so the final exchange
//! does not depend on what was forwarded.

use super::*;
use crate::providers::sse::line_stream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardState {
    /// No `output_text.delta` seen yet. Items that end before it (reasoning, web_search) do not
    /// freeze forwarding.
    BeforeFirstText,
    /// Forwarding deltas of the first item that produced text.
    Forwarding,
    /// That item ended; nothing more is forwarded for this response.
    Frozen,
}

/// Decides, line by line, which deltas belong to the first output text item.
pub(super) struct OutputTextObserver {
    state: ForwardState,
    current_event: String,
}

impl OutputTextObserver {
    pub(super) fn new() -> Self {
        Self {
            state: ForwardState::BeforeFirstText,
            current_event: String::new(),
        }
    }

    /// Returns the delta text to forward for one SSE line, if any.
    pub(super) fn observe_line(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if let Some(ev) = line.strip_prefix("event:") {
            self.current_event = ev.trim().to_string();
            return None;
        }
        let data = line.strip_prefix("data:")?.trim();
        let parsed: Value = serde_json::from_str(data).ok()?;
        let event = parsed["type"].as_str().unwrap_or(&self.current_event);
        match event {
            "response.output_text.delta" => {
                if self.state == ForwardState::Frozen {
                    return None;
                }
                self.state = ForwardState::Forwarding;
                parsed["delta"].as_str().map(str::to_string)
            }
            "response.output_item.done"
            | "response.output_item.completed"
            | "response.output_item.added" => {
                if self.state == ForwardState::Forwarding {
                    self.state = ForwardState::Frozen;
                }
                None
            }
            _ => None,
        }
    }
}

/// Reads a successful SSE body line by line, forwards first-item deltas to `sink`, and returns
/// the body text (lines joined with `\n`, which `parse_exchange` reads line by line).
pub(super) async fn read_observed_body(
    resp: reqwest::Response,
    sink: &OutputTextSink,
) -> Result<String> {
    let mut lines = line_stream(resp.bytes_stream());
    let mut observer = OutputTextObserver::new();
    let mut body = String::new();
    while let Some(line) = lines.next().await {
        let line = line.context("ChatGPT: failed to read response body")?;
        if let Some(delta) = observer.observe_line(&line) {
            sink(OutputTextEvent::Delta(delta));
        }
        body.push_str(&line);
        body.push('\n');
    }
    Ok(body)
}
