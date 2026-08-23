use opencrab_converter::{migrate_in_place, ConversionReport, IN_PLACE_MIGRATION_ID};
use rusqlite::Connection;
use std::path::Path;

#[derive(Debug)]
pub enum EnsureMigratedError {
    Sql(rusqlite::Error),
    Convert(opencrab_converter::ConverterError),
    NeedsManualMigration,
}

impl std::fmt::Display for EnsureMigratedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(error) => write!(formatter, "{error}"),
            Self::Convert(error) => write!(formatter, "{error}"),
            Self::NeedsManualMigration => write!(
                formatter,
                "schema_migration_state has no inplace-v1 and legacy tables are non-empty; \
                 run opencrab-converter --db <path> --config <toml> --environment <env> \
                 --captured-at <utc-nanos> before serving"
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

impl From<opencrab_converter::ConverterError> for EnsureMigratedError {
    fn from(error: opencrab_converter::ConverterError) -> Self {
        Self::Convert(error)
    }
}

#[derive(Debug)]
pub enum MigrationStatus {
    AlreadyApplied,
    AppliedFresh(ConversionReport),
}

pub fn ensure_migrated(
    conn: &mut Connection,
    config: impl AsRef<Path>,
    environment: impl AsRef<Path>,
    captured_at: i64,
) -> Result<MigrationStatus, EnsureMigratedError> {
    if marker_present(conn)? {
        return Ok(MigrationStatus::AlreadyApplied);
    }
    if legacy_tables_nonempty(conn)? {
        return Err(EnsureMigratedError::NeedsManualMigration);
    }
    let report = migrate_in_place(conn, config, environment, captured_at)?;
    Ok(MigrationStatus::AppliedFresh(report))
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

fn legacy_tables_nonempty(conn: &Connection) -> Result<bool, rusqlite::Error> {
    let mut statement = conn.prepare(
        "SELECT name,sql FROM sqlite_master
         WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let tables = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (name, sql) in tables {
        if name.contains("fts") {
            continue;
        }
        if sql
            .as_deref()
            .is_some_and(|sql| sql.to_ascii_uppercase().contains("VIRTUAL TABLE"))
        {
            continue;
        }
        let count: i64 =
            conn.query_row(&format!("SELECT COUNT(*) FROM \"{name}\""), [], |row| {
                row.get(0)
            })?;
        if count > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn inputs(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let config = dir.join("empty.toml");
        let environment = dir.join("empty.env");
        std::fs::write(&config, "").unwrap();
        std::fs::write(&environment, "").unwrap();
        (config, environment)
    }

    #[test]
    fn marker_present_is_noop() {
        let temporary = tempfile::tempdir().unwrap();
        let (config, environment) = inputs(temporary.path());
        let mut conn = Connection::open_in_memory().unwrap();
        opencrab_db::schema::initialize(&conn).unwrap();
        migrate_in_place(&mut conn, &config, &environment, 1).unwrap();
        let status = ensure_migrated(&mut conn, &config, &environment, 1).unwrap();
        assert!(matches!(status, MigrationStatus::AlreadyApplied));
    }

    #[test]
    fn nonempty_legacy_without_marker_fails_loud() {
        let temporary = tempfile::tempdir().unwrap();
        let (config, environment) = inputs(temporary.path());
        let mut conn = Connection::open_in_memory().unwrap();
        opencrab_db::schema::initialize(&conn).unwrap();
        conn.execute(
            "INSERT INTO agents(agent_id,name,persona_name) VALUES('agent-a','A','p')",
            [],
        )
        .unwrap();
        let error = ensure_migrated(&mut conn, &config, &environment, 1).unwrap_err();
        assert!(matches!(error, EnsureMigratedError::NeedsManualMigration));
    }

    #[test]
    fn fresh_legacy_runs_migrate_in_place() {
        let temporary = tempfile::tempdir().unwrap();
        let (config, environment) = inputs(temporary.path());
        let mut conn = Connection::open_in_memory().unwrap();
        opencrab_db::schema::initialize(&conn).unwrap();
        let status = ensure_migrated(&mut conn, &config, &environment, 1).unwrap();
        assert!(matches!(status, MigrationStatus::AppliedFresh(_)));
        let marked: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migration_state WHERE migration_id=?1",
                [IN_PLACE_MIGRATION_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marked, 1);
    }
}
