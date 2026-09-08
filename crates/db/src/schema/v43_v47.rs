use rusqlite::Connection;

use super::helpers::{column_exists, table_exists};
use super::sql::{SESSION_WATCHES_SQL, TOOL_LOGS_SQL};

/// v43: sessions.policy_json と session_watches / tool_logs を足すだけ（データ移動ゼロ）。
pub(super) fn migrate_v43_transplant_schema(conn: &Connection) -> rusqlite::Result<()> {
    let before_tables = user_table_names(conn)?;
    if !column_exists(conn, "sessions", "policy_json")? {
        conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN policy_json TEXT NOT NULL DEFAULT '{}'",
        )?;
    }
    conn.execute_batch(SESSION_WATCHES_SQL)?;
    conn.execute_batch(TOOL_LOGS_SQL)?;
    assert_v43_invariants(conn, &before_tables)
}

/// v44: agents.subject_id と gate 4 表。文面は V3 §6.1。
/// 既存テストが user_version を戻して再実行するため、適用済みなら no-op。
pub(super) fn migrate_v44_extgate(conn: &Connection) -> rusqlite::Result<()> {
    if !column_exists(conn, "agents", "subject_id")? {
        conn.execute_batch(
            r#"
ALTER TABLE agents ADD COLUMN subject_id INTEGER;

WITH ranked AS (
  SELECT agent_id, ROW_NUMBER() OVER (ORDER BY agent_id) AS n FROM agents
)
UPDATE agents
SET subject_id = (SELECT n FROM ranked WHERE ranked.agent_id = agents.agent_id);
"#,
        )?;
    }
    conn.execute_batch(
        r#"
CREATE UNIQUE INDEX IF NOT EXISTS idx_agents_subject_id ON agents(subject_id);

CREATE TRIGGER IF NOT EXISTS agents_subject_id_insert_guard
BEFORE INSERT ON agents
WHEN NEW.subject_id IS NOT NULL AND NEW.subject_id <= 0
BEGIN SELECT RAISE(ABORT, 'agents.subject_id must be positive'); END;

CREATE TRIGGER IF NOT EXISTS agents_subject_id_assign
AFTER INSERT ON agents
WHEN NEW.subject_id IS NULL
BEGIN
  UPDATE agents
  SET subject_id = (SELECT COALESCE(MAX(subject_id), 0) + 1 FROM agents WHERE agent_id <> NEW.agent_id)
  WHERE agent_id = NEW.agent_id;
END;

CREATE TRIGGER IF NOT EXISTS agents_subject_id_update_guard
BEFORE UPDATE OF subject_id ON agents
WHEN NEW.subject_id IS NULL OR NEW.subject_id <= 0
BEGIN SELECT RAISE(ABORT, 'agents.subject_id must be positive'); END;

CREATE TABLE IF NOT EXISTS gate_instances (
  instance_id   TEXT PRIMARY KEY,
  kind_id       TEXT NOT NULL CHECK(length(kind_id) > 0),
  subject_id    INTEGER NOT NULL REFERENCES agents(subject_id) ON DELETE RESTRICT,
  revision      INTEGER NOT NULL CHECK(revision > 0),
  enabled       INTEGER NOT NULL CHECK(enabled IN (0,1)),
  config_b64    TEXT NOT NULL,
  config_digest TEXT NOT NULL CHECK(length(config_digest) = 64),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  deleted_at    INTEGER
);

CREATE TABLE IF NOT EXISTS gate_bindings (
  binding_id TEXT PRIMARY KEY,
  instance_id TEXT NOT NULL REFERENCES gate_instances(instance_id) ON DELETE RESTRICT,
  address TEXT NOT NULL CHECK(length(address) > 0),
  created_at INTEGER NOT NULL,
  closed_at INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_gate_bindings_open_address
ON gate_bindings(instance_id, address) WHERE closed_at IS NULL;

CREATE TABLE IF NOT EXISTS external_origins (
  binding_id TEXT NOT NULL REFERENCES gate_bindings(binding_id) ON DELETE RESTRICT,
  origin TEXT NOT NULL CHECK(length(origin) > 0),
  seq INTEGER NOT NULL CHECK(seq > 0),
  PRIMARY KEY(binding_id, origin),
  UNIQUE(binding_id, seq)
);

CREATE TABLE IF NOT EXISTS deliveries (
  delivery_id TEXT PRIMARY KEY,
  binding_id TEXT NOT NULL REFERENCES gate_bindings(binding_id) ON DELETE RESTRICT,
  payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
  state TEXT NOT NULL CHECK(state IN ('sending','delivered','failed','indeterminate')),
  error TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  CHECK(COALESCE(json_type(payload_json), '') = 'object'),
  CHECK(COALESCE(json_type(payload_json, '$.text'), '') = 'text'),
  CHECK(COALESCE(length(json_extract(payload_json, '$.text')), 0) > 0),
  CHECK(
    (state IN ('sending','delivered') AND error IS NULL) OR
    (state = 'failed' AND error = 'external_rejected') OR
    (state = 'indeterminate' AND error IN ('disconnect','stale sending recovered after restart'))
  )
);
"#,
    )
}

pub(super) fn migrate_v47_gateway_operations(conn: &Connection) -> rusqlite::Result<()> {
    // 宣言 digest は instance 単位で永続（DI-04）。同一 revision の再接続・restart 後も
    // mismatch を拒否できるよう DB に持つ。NULL = 当該 revision で未確立。
    if !column_exists(conn, "gate_instances", "operation_declaration_digest")? {
        conn.execute_batch(
            "ALTER TABLE gate_instances ADD COLUMN operation_declaration_digest TEXT",
        )?;
    }
    // generic operation call。operation / payload / result は opaque 値で、operation ごとの
    // 列・table・CHECK を足さない（§7.1）。成功時の JSON null は result_json='null'（SQL NULL
    // ではない）。合法遷移は sending→succeeded|failed|indeterminate のみ。
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS gateway_operation_calls (
  call_id       TEXT PRIMARY KEY,
  binding_id    TEXT NOT NULL REFERENCES gate_bindings(binding_id) ON DELETE RESTRICT,
  operation     TEXT NOT NULL CHECK(length(operation) > 0),
  payload_json  TEXT NOT NULL CHECK(json_valid(payload_json)),
  result_json   TEXT CHECK(result_json IS NULL OR json_valid(result_json)),
  state         TEXT NOT NULL CHECK(state IN ('sending','succeeded','failed','indeterminate')),
  error         TEXT,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  CHECK(
    (state='sending' AND result_json IS NULL AND error IS NULL) OR
    (state='succeeded' AND result_json IS NOT NULL AND error IS NULL) OR
    (state='failed' AND result_json IS NULL AND error='operation_rejected') OR
    (state='indeterminate' AND result_json IS NULL
      AND error IN ('disconnect','stale sending recovered after restart'))
  )
);
"#,
    )
}

pub(super) fn migrate_v45_nostr_bundle_state(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS nostr_bundle_state (
    binding_id TEXT NOT NULL,
    bundle_id TEXT NOT NULL,
    manifest_json TEXT NOT NULL,
    received_bits TEXT NOT NULL,
    new_admitted_bits TEXT NOT NULL,
    completed INTEGER NOT NULL CHECK(completed IN (0,1)),
    PRIMARY KEY(binding_id, bundle_id)
);
"#,
    )
}

fn migration_err(msg: &str) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
        Some(msg.to_string()),
    )
}

fn user_table_names(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master
         WHERE type='table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    let names = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(names)
}

pub(super) fn expected_v43_user_tables(before: &[String]) -> Vec<String> {
    let mut expected = before.to_vec();
    for name in ["session_watches", "tool_logs"] {
        if !expected.iter().any(|t| t == name) {
            expected.push(name.to_string());
        }
    }
    expected.sort();
    expected
}

/// 同じ TX 内の構造不変（設計 §1.6 / §5.2 の TX 内で閉じるもの）。
/// migration chain全体の構造はsynthetic fixtureのunit testsで検証する。
fn assert_v43_invariants(conn: &Connection, before_tables: &[String]) -> rusqlite::Result<()> {
    let after = user_table_names(conn)?;
    let expected = expected_v43_user_tables(before_tables);
    if after != expected {
        return Err(migration_err(&format!(
            "v43: user tables != expected closed set (got {after:?}, expected {expected:?})"
        )));
    }
    if !table_exists(conn, "sessions")? {
        return Err(migration_err("v43: sessions が消えた"));
    }
    if !table_exists(conn, "agent_sessions")? {
        return Err(migration_err("v43: agent_sessions が消えた"));
    }
    let view_sessions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name='sessions'",
        [],
        |r| r.get(0),
    )?;
    if view_sessions > 0 {
        return Err(migration_err("v43: VIEW sessions が作られた"));
    }
    if !column_exists(conn, "sessions", "policy_json")? {
        return Err(migration_err("v43: sessions.policy_json が無い"));
    }
    let non_default_policy: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sessions WHERE policy_json IS NULL OR policy_json != '{}'",
        [],
        |r| r.get(0),
    )?;
    if non_default_policy > 0 {
        return Err(migration_err("v43: sessions.policy_json が '{}' 以外"));
    }
    let tool_n: i64 = conn.query_row("SELECT COUNT(*) FROM tool_logs", [], |r| r.get(0))?;
    if tool_n != 0 {
        return Err(migration_err("v43: tool_logs が空でない"));
    }
    let watch_n: i64 = conn.query_row("SELECT COUNT(*) FROM session_watches", [], |r| r.get(0))?;
    if watch_n != 0 {
        return Err(migration_err("v43: session_watches が空でない"));
    }
    Ok(())
}
