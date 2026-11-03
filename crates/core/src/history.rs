//! D-1111: the single writer of persisted conversation history (`memory_sessions`).
//!
//! Every production event that becomes a history row is written by [`record`]: one call, one
//! row, written when the event happens. Each [`Event`] builds exactly the row its former writer
//! built (same `log_type`, content, speaker and metadata), so stored data, search and memory are
//! unchanged. Tests and the import CLI keep using `opencrab_db` directly; a guard test fails if
//! any other production code calls `insert_session_log`.
//!
//! The in-turn request view of a running turn has its own single writer
//! (`engine::skill_engine::turn_view`); it appends the same events at the end of the request.

use opencrab_db::queries::SessionLogRow;
use rusqlite::Connection;
use serde_json::Value;

/// One history event. Fields carry what the former writer put in the row.
#[derive(Debug, Clone)]
pub enum Event<'a> {
    /// Speech from someone other than the agent (gateway said, REST, web, intake source).
    InboundSpeech {
        speaker_id: &'a str,
        text: &'a str,
        metadata: Option<Value>,
    },
    /// The agent's own speech (delivered reply, continuation, holding, prelude, utterance op,
    /// the delivered part of an interrupted say, REST/intake replies).
    AgentSpeech {
        text: &'a str,
        metadata: Option<Value>,
    },
    /// A tool call record of the agent (content is the generation's text, if any).
    ToolCall { content: &'a str, metadata: Value },
    /// A tool result record (content already sanitised/capped by the caller).
    ToolResult { content: &'a str, metadata: Value },
    /// A `system` row: JSON event content (turn end, subtask spawned/progress/completed,
    /// delivery cut) or a plain note.
    System {
        content: String,
        speaker_id: Option<&'a str>,
        metadata: Option<Value>,
    },
    /// Any other typed row (`tool_cancelled`, `steer`, `inner_voice`, `task_event`).
    Typed {
        log_type: &'a str,
        content: &'a str,
        speaker_id: Option<&'a str>,
        metadata: Option<Value>,
    },
}

impl Event<'_> {
    /// The exact row this event is stored as.
    pub fn row(&self, agent_id: &str, session_id: &str) -> SessionLogRow {
        let (log_type, content, speaker_id, metadata): (
            &str,
            String,
            Option<String>,
            Option<&Value>,
        ) = match self {
            Event::InboundSpeech {
                speaker_id,
                text,
                metadata,
            } => (
                "speech",
                text.to_string(),
                Some(speaker_id.to_string()),
                metadata.as_ref(),
            ),
            Event::AgentSpeech { text, metadata } => (
                "speech",
                text.to_string(),
                Some(agent_id.to_string()),
                metadata.as_ref(),
            ),
            Event::ToolCall { content, metadata } => (
                "tool_call",
                content.to_string(),
                Some(agent_id.to_string()),
                Some(metadata),
            ),
            Event::ToolResult { content, metadata } => (
                "tool_result",
                content.to_string(),
                Some(agent_id.to_string()),
                Some(metadata),
            ),
            Event::System {
                content,
                speaker_id,
                metadata,
            } => (
                "system",
                content.clone(),
                speaker_id.map(str::to_string),
                metadata.as_ref(),
            ),
            Event::Typed {
                log_type,
                content,
                speaker_id,
                metadata,
            } => (
                log_type,
                content.to_string(),
                speaker_id.map(str::to_string),
                metadata.as_ref(),
            ),
        };
        SessionLogRow {
            id: None,
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            log_type: log_type.to_string(),
            content,
            speaker_id,
            turn_number: None,
            metadata_json: metadata.map(Value::to_string),
            created_at: None,
        }
    }
}

/// Writes one event as one history row now. Inside an open transaction it joins it.
pub fn record(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
    event: &Event<'_>,
) -> anyhow::Result<i64> {
    write_row(conn, &event.row(agent_id, session_id))
}

/// Writes an already built row (a row held until a provisional run is promoted).
pub fn write_row(conn: &Connection, row: &SessionLogRow) -> anyhow::Result<i64> {
    opencrab_db::queries::insert_session_log(conn, row)
}

/// [`record`] for paths where a lost row must not stop the flow: the failure is logged.
pub fn record_best_effort(conn: &Connection, agent_id: &str, session_id: &str, event: &Event<'_>) {
    let row = event.row(agent_id, session_id);
    if let Err(e) = write_row(conn, &row) {
        tracing::warn!(
            session_id = %row.session_id,
            log_type = %row.log_type,
            "session log insert failed (best-effort path): {e}"
        );
    }
}

#[cfg(test)]
mod tests;
