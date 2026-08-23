use opencrab_converter::{
    migrate_in_place_with, MigrationInstanceAssembler, MigrationInstanceTarget,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::FileTimes;
use std::process::Command;
use std::time::{Duration, SystemTime};

const FIXTURE_CAPTURED_AT: i64 = 1_770_000_000_123_456_789;

#[test]
fn phase1_dirty_fixture_in_place_is_accounted_and_row_stable() {
    let temporary = tempfile::tempdir().unwrap();
    let first_db = temporary.path().join("copy-a.db");
    let second_db = temporary.path().join("copy-b.db");
    for path in [&first_db, &second_db] {
        Connection::open(path)
            .unwrap()
            .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
            .unwrap();
    }

    let first = run_in_place(&first_db);
    let second = run_in_place(&second_db);
    assert_eq!(first, second, "accounting report must be deterministic");

    let first_snapshot = snapshot(&first_db, &first);
    let second_snapshot = snapshot(&second_db, &second);
    assert_eq!(
        first_snapshot, second_snapshot,
        "two pristine copies must produce the same new-table row set"
    );
    assert_eq!(first_snapshot, include_str!("fixtures/phase1-golden.txt"));

    let target = Connection::open(&first_db).unwrap();
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM migration_provenance WHERE target_entity='subjects'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        5
    );
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM schema_migration_state WHERE migration_id='inplace-v1'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM subject_history_sources
                 WHERE live_place_id=(SELECT place_id FROM place_source_refs
                                      WHERE source_system='discord' AND source_address='222')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        3,
        "both agents sharing one live address/session need separate history links"
    );
}

#[test]
fn in_place_rejects_a_multiple_interaction_exact_join() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    let connection = Connection::open(&db).unwrap();
    connection
        .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
        .unwrap();
    connection
        .execute(
            "INSERT INTO pending_interactions VALUES(
               'ui-responded','agent_alpha','discord-agent_alpha-111-222','222','msg-duplicate',
               'discord','surface-b','[{\"type\":\"button\",\"id\":\"ok\"}]','responded',
               '{\"surface_id\":\"surface-b\",\"component_id\":\"ok\",\"action_name\":\"submit\",\"context\":null,\"responder_id\":\"principal-1\"}',
               'principal-1',0,30,'2024-01-05 00:03:00','2024-01-05 00:03:10','2024-01-05 00:03:10'
             )",
            [],
        )
        .unwrap();
    drop(connection);
    let error = run_in_place_err(&db);
    assert!(error.contains("history row 8 interaction join is multiple"));
}

#[test]
fn in_place_rejects_a_multiple_task_exact_join() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    let connection = Connection::open(&db).unwrap();
    connection
        .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
        .unwrap();
    connection
        .execute(
            "INSERT INTO task_ledger VALUES(
               1,'agent_alpha','discord-agent_alpha-111-222','Duplicate Synthetic Goal',NULL,
               'active','2024-01-05 00:00:00','2024-01-05 00:00:00',0
             )",
            [],
        )
        .unwrap();
    drop(connection);
    let error = run_in_place_err(&db);
    assert!(error.contains("history row 13 task join is multiple"));
}

#[test]
fn public_cli_uses_explicit_config_and_dotenv_snapshot_reproducibly() {
    let temporary = tempfile::tempdir().unwrap();
    let first = temporary.path().join("copy-a.db");
    let second = temporary.path().join("copy-b.db");
    for path in [&first, &second] {
        Connection::open(path)
            .unwrap()
            .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
            .unwrap();
    }
    let config = temporary.path().join("default.toml");
    let environment = temporary.path().join("cutover.env");
    std::fs::write(
        &config,
        r#"[llm]
default_model = "synthetic-model"
"#,
    )
    .unwrap();
    std::fs::write(&environment, "OPENCRAB_DEFAULT_MODEL=ignored\n").unwrap();
    let first_out = run_cli(&first, &config, &environment, FIXTURE_CAPTURED_AT);
    let second_out = run_cli(&second, &config, &environment, FIXTURE_CAPTURED_AT);
    assert_eq!(first_out, second_out);
}

#[test]
fn public_cli_uses_explicit_time_and_ignores_database_mtime() {
    let temporary = tempfile::tempdir().unwrap();
    let first = temporary.path().join("copy-a.db");
    let second = temporary.path().join("copy-b.db");
    for path in [&first, &second] {
        Connection::open(path)
            .unwrap()
            .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
            .unwrap();
    }
    let later = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
    std::fs::File::options()
        .write(true)
        .open(&second)
        .unwrap()
        .set_times(FileTimes::new().set_modified(later))
        .unwrap();
    let config = temporary.path().join("default.toml");
    let environment = temporary.path().join("snapshot.env");
    std::fs::write(&config, "").unwrap();
    std::fs::write(&environment, "").unwrap();
    let first_out = run_cli(&first, &config, &environment, FIXTURE_CAPTURED_AT);
    let second_out = run_cli(&second, &config, &environment, FIXTURE_CAPTURED_AT);
    assert_eq!(first_out, second_out);
}

struct FixtureInstanceAssembly;

impl MigrationInstanceAssembler for FixtureInstanceAssembly {
    fn assemble(
        &self,
        source: &Connection,
        target: &MigrationInstanceTarget<'_, '_>,
    ) -> opencrab_converter::Result<()> {
        assert!(
            !source.is_autocommit(),
            "instance assembly must share migrate_in_place's open transaction"
        );
        assert_eq!(
            source.query_row("SELECT COUNT(*) FROM trusted_users", [], |row| {
                row.get::<_, i64>(0)
            })?,
            8
        );
        target.create_instance(
            "22222222-2222-4222-8222-222222222222",
            "discord",
            "fixture-discord-b",
            None,
        )?;
        target.create_instance(
            "33333333-3333-4333-8333-333333333333",
            "web",
            "fixture-web",
            None,
        )?;
        target.create_instance(
            "11111111-1111-4111-8111-111111111111",
            "discord",
            "fixture-discord-a",
            None,
        )?;
        Ok(())
    }
}

fn write_empty_inputs(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let config = dir.join("snapshot-config.toml");
    let environment = dir.join("snapshot.env");
    if !config.exists() {
        std::fs::write(&config, "").unwrap();
    }
    if !environment.exists() {
        std::fs::write(&environment, "").unwrap();
    }
    (config, environment)
}

fn run_in_place(db: &std::path::Path) -> String {
    let (config, environment) = write_empty_inputs(db.parent().unwrap());
    let mut conn = Connection::open(db).unwrap();
    migrate_in_place_with(
        &mut conn,
        &config,
        &environment,
        FIXTURE_CAPTURED_AT,
        &FixtureInstanceAssembly,
    )
    .unwrap()
    .to_pretty_json()
    .unwrap()
}

fn run_in_place_err(db: &std::path::Path) -> String {
    let (config, environment) = write_empty_inputs(db.parent().unwrap());
    let mut conn = Connection::open(db).unwrap();
    migrate_in_place_with(
        &mut conn,
        &config,
        &environment,
        FIXTURE_CAPTURED_AT,
        &FixtureInstanceAssembly,
    )
    .unwrap_err()
    .to_string()
}

fn run_cli(
    db: &std::path::Path,
    config: &std::path::Path,
    environment: &std::path::Path,
    captured_at: i64,
) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_opencrab-converter"))
        .args(["--db", db.to_str().unwrap()])
        .args(["--config", config.to_str().unwrap()])
        .args(["--environment", environment.to_str().unwrap()])
        .args(["--captured-at", &captured_at.to_string()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cli failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn snapshot(path: &std::path::Path, report: &str) -> String {
    let connection = Connection::open(path).unwrap();
    let mut output = String::new();
    output.push_str("ACCOUNTING\n");
    let report: serde_json::Value = serde_json::from_str(report).unwrap();
    for class in report["classes"].as_array().unwrap() {
        writeln!(
            output,
            "{}|{}|{}|{}|{}|{}|{}|{}",
            class["source_table"].as_str().unwrap(),
            class["logical_class"].as_str().unwrap(),
            class["source_rows"].as_u64().unwrap(),
            class["canonical_outcomes"].as_u64().unwrap(),
            class["raw_outcomes"].as_u64().unwrap(),
            class["dropped_outcomes"].as_u64().unwrap(),
            class["exact_one_violations"].as_u64().unwrap(),
            class["drop_reasons"],
        )
        .unwrap();
    }
    output.push_str("COUNTS\n");
    append_query(
        &connection,
        "SELECT
           (SELECT COUNT(*) FROM subjects),
           (SELECT COUNT(*) FROM gate_instances),
           (SELECT COUNT(*) FROM places),
           (SELECT COUNT(*) FROM gate_bindings),
           (SELECT COUNT(*) FROM events),
           (SELECT COUNT(*) FROM private_journal),
           (SELECT COUNT(*) FROM legacy_audit_records),
           (SELECT COUNT(*) FROM interactions),
           (SELECT COUNT(*) FROM legacy_unowned_source_rows),
           (SELECT COUNT(*) FROM schema_migration_state)",
        &mut output,
        10,
    );
    output.push_str("SUBJECTS\n");
    append_query(
        &connection,
        "SELECT id,kind,name,persona,turn_runner,standing,printf('%016x',created_at)
         FROM subjects ORDER BY id",
        &mut output,
        7,
    );
    output.push_str("IDENTITIES\n");
    append_query(
        &connection,
        "SELECT instance_id,external_id,subject_id,display_name FROM gate_subject_identities ORDER BY instance_id,external_id",
        &mut output,
        4,
    );
    output.push_str("RUNTIME_CONFIGS\n");
    append_query(
        &connection,
        "SELECT subject_id,model_alias,history_policy,output_policy FROM subject_runtime_configs ORDER BY subject_id",
        &mut output,
        4,
    );
    output.push_str("GATE_CONFIGS\n");
    append_query(
        &connection,
        "SELECT gi.kind_id,gi.owner_subject_id,r.enabled,r.config_schema_id,length(sv.value),sv.at_rest_format
         FROM gate_instances gi
         JOIN gate_instance_revisions r ON r.instance_id=gi.instance_id AND r.revision=gi.active_revision
         LEFT JOIN secret_values sv ON sv.secret_set_id=r.secret_set_id
         ORDER BY gi.kind_id,gi.owner_subject_id,gi.instance_id",
        &mut output,
        6,
    );
    output.push_str("PLACES\n");
    append_query(
        &connection,
        "SELECT p.id,p.address,r.classification,r.source_system,r.source_address
         FROM places p LEFT JOIN place_source_refs r ON r.place_id=p.id ORDER BY p.id",
        &mut output,
        5,
    );
    output.push_str("POLICIES\n");
    append_query(
        &connection,
        "SELECT place_id,kind_id,subject_id,admission,readable,writable,whitelisted,
                heartbeat_enabled,heartbeat_interval_secs,heartbeat_instructions
         FROM place_subject_policies ORDER BY place_id,kind_id,subject_id",
        &mut output,
        10,
    );
    output.push_str("BINDINGS\n");
    append_query(
        &connection,
        "SELECT b.place_id,gi.kind_id,gi.owner_subject_id,b.address,b.binding_metadata_schema_id
         FROM gate_bindings b JOIN gate_instances gi ON gi.instance_id=b.instance_id
         ORDER BY b.place_id,gi.kind_id,gi.owner_subject_id,b.instance_id",
        &mut output,
        5,
    );
    output.push_str("ROUTES\n");
    append_query(
        &connection,
        "SELECT subject_id,place_id,kind_id,purpose FROM subject_routes
         ORDER BY subject_id,place_id,kind_id,purpose",
        &mut output,
        4,
    );
    output.push_str("INTERACTIONS\n");
    append_query(
        &connection,
        "SELECT i.id,i.source_record_key,i.state,i.owner_subject_id,i.place_id,
                r.responder_kind,r.responder_external_id
         FROM interactions i LEFT JOIN interaction_responses r ON r.interaction_id=i.id
         ORDER BY i.id",
        &mut output,
        7,
    );
    output.push_str("EVENTS\n");
    append_query(
        &connection,
        "SELECT place_id,seq,kind,author_subject_id,author_external_id,content_json,mentions_json
         FROM events ORDER BY place_id,seq",
        &mut output,
        7,
    );
    output.push_str("JOURNAL_AUDIT\n");
    append_query(
        &connection,
        "SELECT 'journal',owner_subject_id,place_id,CAST(content AS TEXT) FROM private_journal
         UNION ALL
         SELECT audit_kind,owner_subject_id,place_id,CAST(content AS TEXT) FROM legacy_audit_records
         ORDER BY 1,4",
        &mut output,
        4,
    );
    output.push_str("HISTORY_SOURCES\n");
    append_query(
        &connection,
        "SELECT subject_id,live_place_id,history_place_id,ordinal,history_max_seq
         FROM subject_history_sources ORDER BY subject_id,live_place_id,ordinal",
        &mut output,
        5,
    );
    output.push_str("PROVENANCE_GRANTS\n");
    append_query(
        &connection,
        "SELECT agent_subject_id,principal_subject_id,gate_kind,source_permission
         FROM grant_source_provenance ORDER BY agent_subject_id,principal_subject_id,gate_kind,rowid",
        &mut output,
        4,
    );
    output.push_str("RAW\n");
    let mut statement = connection
        .prepare(
            "SELECT source_table,source_key,row_values,reason
             FROM legacy_unowned_source_rows ORDER BY source_table,source_key",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap();
    for row in rows {
        let (table, key, values, reason) = row.unwrap();
        writeln!(
            output,
            "{table}|{}|{}|{reason}",
            hex(&key),
            hex(&Sha256::digest(values))
        )
        .unwrap();
    }
    output
}

fn append_query(connection: &Connection, sql: &str, output: &mut String, columns: usize) {
    let mut statement = connection.prepare(sql).unwrap();
    let rows = statement
        .query_map([], |row| {
            (0..columns)
                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<std::result::Result<Vec<_>, _>>()
        })
        .unwrap();
    for row in rows {
        let fields = row
            .unwrap()
            .into_iter()
            .map(|value| match value {
                rusqlite::types::Value::Null => "NULL".into(),
                rusqlite::types::Value::Integer(value) => value.to_string(),
                rusqlite::types::Value::Real(value) => format!("{value:?}"),
                rusqlite::types::Value::Text(value) => value,
                rusqlite::types::Value::Blob(value) => hex(&value),
            })
            .collect::<Vec<_>>();
        writeln!(output, "{}", fields.join("|")).unwrap();
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
