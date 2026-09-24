use rusqlite::{Connection, OptionalExtension};

use super::Migration;

pub(super) static MIGRATIONS: &[Migration] = &[Migration {
    version: 54,
    description: "add monotonic subjects, tombstones, grants, and binding authority state",
    up: migrate_subject_safeguards,
}];

fn migrate_subject_safeguards(conn: &Connection) -> rusqlite::Result<()> {
    validate_existing_subjects(conn)?;

    let adds_grandfathered = !column_exists(conn, "gate_instances", "association_grandfathered")?;
    if adds_grandfathered {
        conn.execute_batch(
            "ALTER TABLE gate_instances ADD COLUMN association_grandfathered INTEGER NOT NULL DEFAULT 0
                 CHECK(association_grandfathered IN (0, 1));
             UPDATE gate_instances SET association_grandfathered = 1;",
        )?;
    }

    let adds_session_id = !column_exists(conn, "gate_bindings", "session_id")?;
    if adds_session_id {
        conn.execute_batch("ALTER TABLE gate_bindings ADD COLUMN session_id TEXT;")?;
        conn.execute_batch(
            "UPDATE gate_bindings
             SET session_id = CASE
                 WHEN EXISTS (
                     SELECT 1 FROM sessions
                     WHERE sessions.id = 'extgate-' || gate_bindings.binding_id
                 ) THEN 'extgate-' || binding_id
                 ELSE address
             END;",
        )?;
    }

    conn.execute_batch(SUBJECT_SAFEGUARDS_SQL)?;
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM pragma_table_info(?1) WHERE name=?2",
        [table, column],
        |_| Ok(true),
    )
    .optional()
    .map(|value| value.unwrap_or(false))
}

fn validate_existing_subjects(conn: &Connection) -> rusqlite::Result<()> {
    let invalid_agents: i64 = conn.query_row(
        "SELECT count(*) FROM agents
         WHERE subject_id IS NULL OR typeof(subject_id) <> 'integer' OR subject_id <= 0",
        [],
        |row| row.get(0),
    )?;
    let invalid_associations: i64 = conn.query_row(
        "SELECT count(*) FROM gate_instances AS instance
         LEFT JOIN agents AS agent ON agent.subject_id = instance.subject_id
         WHERE typeof(instance.subject_id) <> 'integer'
            OR instance.subject_id <= 0
            OR agent.agent_id IS NULL",
        [],
        |row| row.get(0),
    )?;
    if invalid_agents != 0 || invalid_associations != 0 {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("S2 requires positive unique agent subjects and valid gate associations".into()),
        ));
    }
    Ok(())
}

const SUBJECT_SAFEGUARDS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS subject_id_allocator (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    next_subject_id INTEGER NOT NULL CHECK(next_subject_id > 0)
);
INSERT INTO subject_id_allocator(singleton, next_subject_id)
SELECT 1, COALESCE(MAX(subject_id), 0) + 1 FROM agents
WHERE NOT EXISTS(SELECT 1 FROM subject_id_allocator);
UPDATE subject_id_allocator
SET next_subject_id = (SELECT COALESCE(MAX(subject_id), 0) + 1 FROM agents)
WHERE singleton = 1
  AND next_subject_id <= (SELECT COALESCE(MAX(subject_id), 0) FROM agents);

CREATE TABLE IF NOT EXISTS subject_tombstones (
    subject_id INTEGER PRIMARY KEY CHECK(subject_id > 0),
    deleted_at INTEGER NOT NULL,
    deletion_revision INTEGER NOT NULL CHECK(deletion_revision > 0)
);

CREATE TABLE IF NOT EXISTS subject_association_grants (
    grant_hash BLOB PRIMARY KEY CHECK(length(grant_hash) = 64),
    agent_id TEXT NOT NULL CHECK(length(agent_id) > 0),
    subject_id INTEGER NOT NULL CHECK(subject_id > 0),
    expires_at INTEGER NOT NULL,
    consumed_at INTEGER,
    consumed_instance_id TEXT,
    CHECK((consumed_at IS NULL) = (consumed_instance_id IS NULL))
);
CREATE INDEX IF NOT EXISTS idx_subject_association_grants_pair
    ON subject_association_grants(agent_id, subject_id, expires_at);

CREATE TRIGGER IF NOT EXISTS subject_allocator_no_delete
BEFORE DELETE ON subject_id_allocator
BEGIN SELECT RAISE(ABORT, 'subject allocator is permanent'); END;
CREATE TRIGGER IF NOT EXISTS subject_allocator_monotonic
BEFORE UPDATE ON subject_id_allocator
WHEN NEW.singleton <> OLD.singleton OR NEW.next_subject_id <= OLD.next_subject_id
BEGIN SELECT RAISE(ABORT, 'subject allocator cannot move backward'); END;

CREATE TRIGGER IF NOT EXISTS subject_tombstones_no_update
BEFORE UPDATE ON subject_tombstones
BEGIN SELECT RAISE(ABORT, 'subject tombstones are permanent'); END;
CREATE TRIGGER IF NOT EXISTS subject_tombstones_no_delete
BEFORE DELETE ON subject_tombstones
BEGIN SELECT RAISE(ABORT, 'subject tombstones are permanent'); END;

CREATE TRIGGER IF NOT EXISTS subject_grants_no_delete
BEFORE DELETE ON subject_association_grants
BEGIN SELECT RAISE(ABORT, 'subject association grants are permanent'); END;
CREATE TRIGGER IF NOT EXISTS subject_grants_consume_once
BEFORE UPDATE ON subject_association_grants
WHEN NOT (
    OLD.consumed_at IS NULL AND OLD.consumed_instance_id IS NULL
    AND NEW.consumed_at IS NOT NULL AND NEW.consumed_instance_id IS NOT NULL
    AND NEW.grant_hash IS OLD.grant_hash
    AND NEW.agent_id IS OLD.agent_id
    AND NEW.subject_id IS OLD.subject_id
    AND NEW.expires_at IS OLD.expires_at
)
BEGIN SELECT RAISE(ABORT, 'subject association grants are single-use'); END;

DROP TRIGGER IF EXISTS agents_subject_id_insert_guard;
DROP TRIGGER IF EXISTS agents_subject_id_assign;
DROP TRIGGER IF EXISTS agents_subject_id_update_guard;

CREATE TRIGGER agents_subject_id_insert_guard
BEFORE INSERT ON agents
WHEN NEW.subject_id IS NOT NULL AND (
    typeof(NEW.subject_id) <> 'integer'
    OR NEW.subject_id <= 0
    OR NEW.subject_id < (SELECT next_subject_id FROM subject_id_allocator WHERE singleton=1)
    OR EXISTS(SELECT 1 FROM subject_tombstones WHERE subject_id=NEW.subject_id)
)
BEGIN SELECT RAISE(ABORT, 'agents.subject_id must be a fresh positive integer'); END;

CREATE TRIGGER agents_subject_id_assign
AFTER INSERT ON agents
WHEN NEW.subject_id IS NULL
BEGIN
    UPDATE agents
    SET subject_id = (SELECT next_subject_id FROM subject_id_allocator WHERE singleton=1)
    WHERE agent_id = NEW.agent_id;
    UPDATE subject_id_allocator
    SET next_subject_id = next_subject_id + 1
    WHERE singleton=1;
END;

CREATE TRIGGER agents_subject_id_advance_explicit
AFTER INSERT ON agents
WHEN NEW.subject_id IS NOT NULL
BEGIN
    UPDATE subject_id_allocator
    SET next_subject_id = NEW.subject_id + 1
    WHERE singleton=1 AND next_subject_id <= NEW.subject_id;
END;

CREATE TRIGGER agents_subject_id_update_guard
BEFORE UPDATE OF subject_id ON agents
WHEN NOT (
    OLD.subject_id IS NULL
    AND NEW.subject_id = (SELECT next_subject_id FROM subject_id_allocator WHERE singleton=1)
)
BEGIN SELECT RAISE(ABORT, 'agents.subject_id is immutable'); END;

CREATE TRIGGER agents_subject_tombstone_delete_guard
BEFORE DELETE ON agents
WHEN NOT EXISTS(
    SELECT 1 FROM subject_tombstones WHERE subject_id=OLD.subject_id
)
BEGIN SELECT RAISE(ABORT, 'subject must be tombstoned before hard delete'); END;
"#;
