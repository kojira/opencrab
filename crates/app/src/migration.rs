use opencrab_converter::IN_PLACE_MIGRATION_ID;
use rusqlite::Connection;

/// Body-legacy tables: names the old implementation created that store SCHEMA does not.
/// Existence (not row counts) is the sentinel. Any one present means a legacy-implementation DB.
const BODY_LEGACY_SENTINELS: &[&str] = &["agents", "sessions", "skills"];

#[derive(Debug)]
pub enum EnsureMigratedError {
    Sql(rusqlite::Error),
    NeedsManualMigration,
}

impl std::fmt::Display for EnsureMigratedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(error) => write!(formatter, "{error}"),
            Self::NeedsManualMigration => write!(
                formatter,
                "this is a legacy-implementation DB; run the migration"
            ),
        }
    }
}

impl std::error::Error for EnsureMigratedError {}

impl From<rusqlite::Error> for EnsureMigratedError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

/// Startup gate only. Three-way:
/// - body-legacy tables exist and no `inplace-v1` marker → refuse
/// - body-legacy tables exist and marker present → boot
/// - no body-legacy tables → boot (marker is not required; converter is not invoked)
pub fn ensure_migrated(conn: &Connection) -> Result<(), EnsureMigratedError> {
    if body_legacy_tables_exist(conn)? && !marker_present(conn)? {
        return Err(EnsureMigratedError::NeedsManualMigration);
    }
    Ok(())
}

fn body_legacy_tables_exist(conn: &Connection) -> Result<bool, rusqlite::Error> {
    for name in BODY_LEGACY_SENTINELS {
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [*name],
            |row| row.get(0),
        )?;
        if exists > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn marker_present(conn: &Connection) -> Result<bool, rusqlite::Error> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migration_state'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(false);
    }
    let marked: i64 = conn.query_row(
        "SELECT COUNT(*) FROM schema_migration_state WHERE migration_id=?1",
        [IN_PLACE_MIGRATION_ID],
        |row| row.get(0),
    )?;
    Ok(marked > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn marker_row_count(conn: &Connection) -> i64 {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migration_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if exists == 0 {
            return 0;
        }
        conn.query_row(
            "SELECT COUNT(*) FROM schema_migration_state WHERE migration_id=?1",
            [IN_PLACE_MIGRATION_ID],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn create_agents(conn: &Connection) {
        conn.execute("CREATE TABLE agents(agent_id TEXT NOT NULL)", [])
            .unwrap();
    }

    fn write_inplace_v1_marker(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migration_state(
               migration_id TEXT NOT NULL PRIMARY KEY,
               applied_at INTEGER NOT NULL,
               source_row_digest BLOB
             );
             INSERT INTO schema_migration_state(migration_id, applied_at)
             VALUES('inplace-v1', 1);",
        )
        .unwrap();
    }

    #[test]
    fn fresh_empty_db_boots_without_creating_marker() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_migrated(&conn).unwrap();
        assert_eq!(marker_row_count(&conn), 0);
    }

    #[test]
    fn used_new_structure_db_without_marker_boots() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE subjects(
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               kind TEXT NOT NULL,
               name TEXT NOT NULL DEFAULT '',
               persona TEXT NOT NULL,
               turn_runner TEXT NOT NULL,
               standing TEXT NOT NULL,
               created_at INTEGER NOT NULL
             );
             CREATE TABLE events(
               place_id INTEGER NOT NULL,
               seq INTEGER NOT NULL,
               kind TEXT NOT NULL,
               content_json TEXT NOT NULL,
               mentions_json TEXT NOT NULL,
               created_at INTEGER NOT NULL,
               PRIMARY KEY(place_id, seq)
             );
             INSERT INTO subjects(kind,name,persona,turn_runner,standing,created_at)
             VALUES('agent','used','p','echo','trusted',1);
             INSERT INTO events(place_id,seq,kind,content_json,mentions_json,created_at)
             VALUES(1,1,'said','{}','[]',1);",
        )
        .unwrap();
        ensure_migrated(&conn).unwrap();
        assert_eq!(marker_row_count(&conn), 0);
    }

    #[test]
    fn agents_table_without_marker_refuses_with_migration_message() {
        let conn = Connection::open_in_memory().unwrap();
        create_agents(&conn);
        let error = ensure_migrated(&conn).unwrap_err();
        assert!(matches!(error, EnsureMigratedError::NeedsManualMigration));
        assert_eq!(
            error.to_string(),
            "this is a legacy-implementation DB; run the migration"
        );
        assert_eq!(marker_row_count(&conn), 0);
    }

    #[test]
    fn agents_table_with_inplace_v1_marker_boots() {
        let conn = Connection::open_in_memory().unwrap();
        create_agents(&conn);
        write_inplace_v1_marker(&conn);
        ensure_migrated(&conn).unwrap();
        assert_eq!(marker_row_count(&conn), 1);
    }
}
