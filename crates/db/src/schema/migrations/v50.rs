use rusqlite::Connection;

use super::super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 50,
    description: "モデル入力上限を明確化しtool completion eventを永続化する（#975）",
    up: migrate_v50_tool_continuation,
}];

fn migrate_v50_tool_continuation(conn: &Connection) -> rusqlite::Result<()> {
    if !super::super::column_exists(conn, "model_pricing", "max_input_tokens")? {
        conn.execute_batch("ALTER TABLE model_pricing ADD COLUMN max_input_tokens INTEGER;")?;
    }
    if !super::super::column_exists(conn, "model_pricing", "max_total_tokens")? {
        conn.execute_batch("ALTER TABLE model_pricing ADD COLUMN max_total_tokens INTEGER;")?;
    }
    conn.execute_batch(
        "UPDATE model_pricing SET max_input_tokens = 1050000
           WHERE provider = 'codex' AND model = 'gpt-5.6' AND max_input_tokens IS NULL;
         UPDATE model_pricing SET max_input_tokens = 350000
           WHERE provider = 'chatgpt'
             AND model IN ('gpt-5.6', 'gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-5.6-luna')
             AND max_input_tokens IS NULL;
         UPDATE model_pricing SET max_input_tokens = 1000000
           WHERE provider = 'hermit' AND model = 'claude-opus-5' AND max_input_tokens IS NULL;
         UPDATE model_pricing SET max_input_tokens = 100000
           WHERE provider = 'cursor' AND model = 'cursor-grok-4.6-high'
             AND max_input_tokens IS NULL;
         CREATE TABLE IF NOT EXISTS tool_call_correlations (
             session_id TEXT NOT NULL,
             sequence INTEGER NOT NULL CHECK (sequence > 0),
             short_id TEXT NOT NULL,
             provider_call_id TEXT NOT NULL,
             created_at TEXT NOT NULL,
             PRIMARY KEY (session_id, short_id),
             UNIQUE (session_id, sequence),
             UNIQUE (session_id, provider_call_id)
         );
         CREATE TABLE IF NOT EXISTS tool_completion_requests (
             request_id TEXT PRIMARY KEY,
             session_id TEXT NOT NULL,
             request_digest TEXT NOT NULL,
             request_json TEXT NOT NULL,
             response_json TEXT,
             effects_state TEXT NOT NULL CHECK (effects_state IN ('awaiting_response', 'pending', 'applied')),
             created_at TEXT NOT NULL,
             applied_at TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_tool_completion_requests_session_effects
             ON tool_completion_requests(session_id, effects_state, created_at);
         CREATE TABLE IF NOT EXISTS tool_completion_events (
             event_id TEXT PRIMARY KEY,
             session_id TEXT NOT NULL,
             causal_turn_id TEXT NOT NULL,
             tool_call_id TEXT NOT NULL,
             execution_id TEXT NOT NULL,
             result_log_id INTEGER NOT NULL,
             state TEXT NOT NULL CHECK (state IN ('queued', 'included', 'consumed')),
             included_request_id TEXT,
             request_digest TEXT,
             completed_at TEXT NOT NULL,
             consumed_at TEXT,
             FOREIGN KEY (result_log_id) REFERENCES memory_sessions(id) ON DELETE CASCADE,
             FOREIGN KEY (included_request_id) REFERENCES tool_completion_requests(request_id),
             UNIQUE (execution_id, tool_call_id)
         );
         CREATE INDEX IF NOT EXISTS idx_tool_completion_events_session_state
             ON tool_completion_events(session_id, state, completed_at, event_id);",
    )
}
