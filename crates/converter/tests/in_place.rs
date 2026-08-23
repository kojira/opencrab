use opencrab_converter::{migrate_in_place, ConverterError, IN_PLACE_MIGRATION_ID};
use rusqlite::Connection;

const CAPTURED_AT: i64 = 1_770_000_000_000_000_000;
const LLM_LOGS_ADDITIVE_COLUMNS: &[&str] =
    &["turn_record_id", "iteration", "place_id", "subject_id"];

#[test]
fn old_tables_stay_byte_stable_except_enumerated_llm_logs_columns() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    let conn = Connection::open(&db).unwrap();
    opencrab_db::schema::initialize(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO agents(
           agent_id,name,persona_name,personality,instructions,heartbeat_instructions,
           created_at,updated_at
         ) VALUES(
           'agent-a','Agent A','persona-a','kind','do work','hb',
           '2024-01-01 00:00:00','2024-01-01 00:00:00'
         );
         INSERT INTO model_pricing(
           provider,model,input_price_per_1m,output_price_per_1m,context_window,updated_at
         ) VALUES('openai','synthetic-model',1,2,128000,'2024-01-01 00:00:00');
         INSERT INTO llm_logs(id,agent_id,prompt,response,is_bot_iteration)
         VALUES('log-1','agent-a','hello','world',0);",
    )
    .unwrap();
    drop(conn);

    let before = capture_legacy(&db);
    run_migrate(&db);
    let after = capture_legacy(&db);

    for name in &before.names {
        assert_eq!(
            before.counts[name], after.counts[name],
            "old table {name} row count must be unchanged"
        );
        if name == "llm_logs" {
            for column in LLM_LOGS_ADDITIVE_COLUMNS {
                assert!(
                    !before.columns[name].iter().any(|c| c == column),
                    "llm_logs fixture must start without {column}"
                );
                assert!(
                    after.columns[name].iter().any(|c| c == column),
                    "llm_logs must gain enumerated column {column}"
                );
            }
        } else {
            assert_eq!(
                before.columns[name], after.columns[name],
                "old table {name} columns must be unchanged"
            );
        }
    }
}

#[test]
fn conversion_report_accounting_balances() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    Connection::open(&db)
        .unwrap()
        .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
        .unwrap();
    let report = run_migrate(&db);
    assert!(report
        .classes
        .iter()
        .all(|class| class.exact_one_violations == 0));
}

#[test]
fn two_pristine_copies_match_new_table_rows() {
    let temporary = tempfile::tempdir().unwrap();
    let first = temporary.path().join("a.db");
    let second = temporary.path().join("b.db");
    for path in [&first, &second] {
        Connection::open(path)
            .unwrap()
            .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
            .unwrap();
    }
    run_migrate(&first);
    run_migrate(&second);
    assert_eq!(new_table_digest(&first), new_table_digest(&second));
}

#[test]
fn migrate_in_place_fails_loud_on_second_run() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    Connection::open(&db)
        .unwrap()
        .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
        .unwrap();
    run_migrate(&db);
    let (config, environment) = write_inputs(temporary.path());
    let mut conn = Connection::open(&db).unwrap();
    let error = migrate_in_place(&mut conn, &config, &environment, CAPTURED_AT).unwrap_err();
    assert!(matches!(error, ConverterError::AlreadyApplied));
}

#[test]
fn fresh_database_writes_marker_and_zero_source_families() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("fresh.db");
    let conn = Connection::open(&db).unwrap();
    opencrab_db::schema::initialize(&conn).unwrap();
    drop(conn);
    let report = run_migrate(&db);
    assert!(report
        .classes
        .iter()
        .all(|class| class.exact_one_violations == 0));
    for class in &report.classes {
        assert_eq!(class.source_rows, 0, "{}", class.source_table);
        assert_eq!(class.canonical_outcomes, 0, "{}", class.logical_class);
        assert_eq!(class.raw_outcomes, 0, "{}", class.logical_class);
    }
    let conn = Connection::open(&db).unwrap();
    let marker: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM schema_migration_state WHERE migration_id=?1",
            [IN_PLACE_MIGRATION_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(marker, 1);
    let subjects: i64 = conn
        .query_row("SELECT COUNT(*) FROM subjects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(subjects, 0);
}

fn write_inputs(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let config = dir.join("empty.toml");
    let environment = dir.join("empty.env");
    std::fs::write(&config, "").unwrap();
    std::fs::write(&environment, "").unwrap();
    (config, environment)
}

fn run_migrate(db: &std::path::Path) -> opencrab_converter::ConversionReport {
    let (config, environment) = write_inputs(db.parent().unwrap());
    let mut conn = Connection::open(db).unwrap();
    migrate_in_place(&mut conn, &config, &environment, CAPTURED_AT).unwrap()
}

struct LegacySnapshot {
    names: Vec<String>,
    counts: std::collections::BTreeMap<String, i64>,
    columns: std::collections::BTreeMap<String, Vec<String>>,
}

fn capture_legacy(path: &std::path::Path) -> LegacySnapshot {
    let conn = Connection::open(path).unwrap();
    let mut statement = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type='table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .unwrap();
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    let mut counts = std::collections::BTreeMap::new();
    let mut columns = std::collections::BTreeMap::new();
    for name in &names {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{name}\""), [], |row| {
                row.get(0)
            })
            .unwrap();
        counts.insert(name.clone(), count);
        columns.insert(name.clone(), table_columns(&conn, name));
    }
    LegacySnapshot {
        names,
        counts,
        columns,
    }
}

fn table_columns(conn: &Connection, name: &str) -> Vec<String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info(\"{name}\")"))
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn new_table_digest(path: &std::path::Path) -> String {
    let conn = Connection::open(path).unwrap();
    let mut statement = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type='table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .unwrap();
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    let mut digest = String::new();
    for name in names {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{name}\""), [], |row| {
                row.get(0)
            })
            .unwrap();
        digest.push_str(&format!("{name}:{count}\n"));
    }
    digest
}
