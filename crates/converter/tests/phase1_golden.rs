use opencrab_converter::{
    convert, ConvertOptions, MigrationInstanceAssembler, MigrationInstanceTarget,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::{File, FileTimes};
use std::process::Command;
use std::time::{Duration, SystemTime};

const FIXTURE_CAPTURED_AT: i64 = 1_770_000_000_123_456_789;

#[test]
fn phase1_dirty_fixture_public_convert_is_accounted_and_byte_stable() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source.db");
    Connection::open(&source)
        .unwrap()
        .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
        .unwrap();

    let first = run_public_convert(&source, &temporary.path().join("target-a.db"));
    let second = run_public_convert(&source, &temporary.path().join("target-b.db"));
    assert_eq!(first.0, second.0, "accounting report must be deterministic");
    assert_eq!(
        std::fs::read(temporary.path().join("target-a.db")).unwrap(),
        std::fs::read(temporary.path().join("target-b.db")).unwrap(),
        "same binary and snapshot must produce byte-identical SQLite output"
    );

    let snapshot = snapshot(&temporary.path().join("target-a.db"), &first.0);
    assert_eq!(snapshot, include_str!("fixtures/phase1-golden.txt"));

    let target = Connection::open(temporary.path().join("target-a.db")).unwrap();
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
                "SELECT COUNT(*) FROM migration_provenance
                 WHERE target_entity='gate_subject_identities'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        7
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
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM subject_history_sources h
                 JOIN memberships m ON m.place_id=h.history_place_id
                 WHERE m.subject_id=h.subject_id AND m.role='participant'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        3,
        "each historical place membership must belong to its linked subject"
    );
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM subject_history_sources h
                 JOIN memberships m ON m.place_id=h.history_place_id
                 WHERE m.subject_id<>h.subject_id",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0,
        "historical memberships must not cross agents"
    );
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM (
                   SELECT subject_id,live_place_id,COUNT(*) AS n,MIN(ordinal) AS lo,MAX(ordinal) AS hi
                   FROM subject_history_sources GROUP BY subject_id,live_place_id
                 ) WHERE lo<>0 OR hi+1<>n",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0,
        "ordinal must be dense from zero independently for each subject"
    );
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM events e
                 JOIN subject_history_sources h ON h.history_place_id=e.place_id
                 WHERE e.author_subject_id IN (1,2) AND e.author_subject_id<>h.subject_id",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0,
        "one agent's self-authored event must never appear in the other's history place"
    );
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind='interrupted'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM private_journal j
                 JOIN place_source_refs p ON p.place_id=j.place_id
                 JOIN places child ON child.id=p.place_id
                 JOIN place_source_refs parent ON parent.place_id=child.parent_id
                 WHERE p.classification='child' AND parent.classification='live'
                   AND CAST(j.content AS TEXT)='change direction'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM legacy_audit_records
                 WHERE scope='task_event' AND activity_id=1 AND reason='task reference resolved'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
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
            "instance assembly must share convert's open source snapshot"
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

fn run_public_convert(source: &std::path::Path, target: &std::path::Path) -> (String, Vec<u8>) {
    let config = source.parent().unwrap().join("snapshot-config.toml");
    let environment = source.parent().unwrap().join("snapshot.env");
    if !config.exists() {
        std::fs::write(&config, "").unwrap();
    }
    if !environment.exists() {
        std::fs::write(&environment, "").unwrap();
    }
    let outcome = convert(
        ConvertOptions {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
            config,
            environment,
            captured_at: FIXTURE_CAPTURED_AT,
        },
        &FixtureInstanceAssembly,
    )
    .unwrap();
    (outcome.report.to_pretty_json().unwrap(), Vec::new())
}

#[test]
fn converter_rejects_every_preexisting_target_even_when_logically_empty() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source.db");
    let target = temporary.path().join("target.db");
    Connection::open(&source)
        .unwrap()
        .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
        .unwrap();
    Connection::open(&target)
        .unwrap()
        .execute_batch(
            "PRAGMA user_version=37;
             CREATE TABLE discarded(x);
             DROP TABLE discarded;",
        )
        .unwrap();
    let config = temporary.path().join("default.toml");
    let environment = temporary.path().join("snapshot.env");
    std::fs::write(&config, "").unwrap();
    std::fs::write(&environment, "").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_opencrab-converter"))
        .args(["--source", source.to_str().unwrap()])
        .args(["--target", target.to_str().unwrap()])
        .args(["--config", config.to_str().unwrap()])
        .args(["--environment", environment.to_str().unwrap()])
        .args(["--captured-at", &FIXTURE_CAPTURED_AT.to_string()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("target database path already exists"));
}

#[test]
fn public_convert_rejects_a_multiple_interaction_exact_join() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source.db");
    let connection = Connection::open(&source).unwrap();
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
    let config = temporary.path().join("default.toml");
    let environment = temporary.path().join("snapshot.env");
    std::fs::write(&config, "").unwrap();
    std::fs::write(&environment, "").unwrap();
    let error = convert(
        ConvertOptions {
            source,
            target: temporary.path().join("target.db"),
            config,
            environment,
            captured_at: FIXTURE_CAPTURED_AT,
        },
        &FixtureInstanceAssembly,
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("history row 8 interaction join is multiple"));
}

#[test]
fn public_convert_rejects_a_multiple_task_exact_join() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source.db");
    let connection = Connection::open(&source).unwrap();
    connection
        .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
        .unwrap();
    connection
        .execute(
            "INSERT INTO task_ledger VALUES(
               1,'agent_alpha','discord-agent_alpha-111-222','Duplicate Synthetic Goal',NULL,
               'active','2024-01-05 00:00:00','2024-01-05 00:00:00'
             )",
            [],
        )
        .unwrap();
    drop(connection);
    let config = temporary.path().join("default.toml");
    let environment = temporary.path().join("snapshot.env");
    std::fs::write(&config, "").unwrap();
    std::fs::write(&environment, "").unwrap();
    let error = convert(
        ConvertOptions {
            source,
            target: temporary.path().join("target.db"),
            config,
            environment,
            captured_at: FIXTURE_CAPTURED_AT,
        },
        &FixtureInstanceAssembly,
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("history row 13 task join is multiple"));
}

#[test]
fn public_cli_uses_explicit_config_and_dotenv_snapshot_reproducibly() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source.db");
    Connection::open(&source)
        .unwrap()
        .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
        .unwrap();
    let config = temporary.path().join("default.toml");
    let environment = temporary.path().join("cutover.env");
    std::fs::write(
        &config,
        r#"[llm]
default_model = "synthetic-model"
compaction_ratio = 0.5
[provider.synthetic]
api_key = "${UNDEFINED_PROVIDER_SECRET}"
[gateway.discord]
enabled = true
token = "${FIXTURE_DISCORD_TOKEN}"
owner_discord_id = "principal-1"
agent_ids = ["agent_alpha", "agent_beta"]
guild_ids = ["111"]
"#,
    )
    .unwrap();
    std::fs::write(&environment, "FIXTURE_DISCORD_TOKEN=dotenv-token\n").unwrap();

    let run = |target: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_opencrab-converter"))
            .args(["--source", source.to_str().unwrap()])
            .args(["--target", target.to_str().unwrap()])
            .args(["--config", config.to_str().unwrap()])
            .args(["--environment", environment.to_str().unwrap()])
            .args(["--captured-at", &FIXTURE_CAPTURED_AT.to_string()])
            .output()
            .unwrap()
    };
    let target_a = temporary.path().join("target-cli-a.db");
    let target_b = temporary.path().join("target-cli-b.db");
    let first = run(&target_a);
    let second = run(&target_b);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(
        std::fs::read(&target_a).unwrap(),
        std::fs::read(&target_b).unwrap()
    );
    let target = Connection::open(target_a).unwrap();
    assert_eq!(
        target
            .query_row(
                "SELECT CAST(value AS TEXT) FROM secret_values WHERE value=CAST('dotenv-token' AS BLOB)",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "dotenv-token"
    );
    let first_authority = target
        .query_row(
            "SELECT hex(source_database_digest) FROM migration_provenance LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    drop(target);

    std::fs::write(&environment, "FIXTURE_DISCORD_TOKEN=changed-token\n").unwrap();
    let target_c = temporary.path().join("target-cli-c.db");
    let third = run(&target_c);
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    let changed = Connection::open(target_c).unwrap();
    let changed_authority = changed
        .query_row(
            "SELECT hex(source_database_digest) FROM migration_provenance LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    assert_ne!(first_authority, changed_authority);
    assert_eq!(
        changed
            .query_row(
                "SELECT COUNT(*) FROM secret_values WHERE value=CAST('changed-token' AS BLOB)",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn public_cli_uses_explicit_time_and_ignores_source_database_mtime() {
    let temporary = tempfile::tempdir().unwrap();
    let seed = temporary.path().join("source-seed.db");
    Connection::open(&seed)
        .unwrap()
        .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
        .unwrap();
    let source_a = temporary.path().join("source-a.db");
    let source_b = temporary.path().join("source-b.db");
    std::fs::copy(&seed, &source_a).unwrap();
    std::fs::copy(&seed, &source_b).unwrap();
    File::options()
        .write(true)
        .open(&source_a)
        .unwrap()
        .set_times(
            FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_196_860)),
        )
        .unwrap();
    File::options()
        .write(true)
        .open(&source_b)
        .unwrap()
        .set_times(
            FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_769_965_320)),
        )
        .unwrap();
    assert_eq!(
        std::fs::read(&source_a).unwrap(),
        std::fs::read(&source_b).unwrap()
    );
    assert_ne!(
        std::fs::metadata(&source_a).unwrap().modified().unwrap(),
        std::fs::metadata(&source_b).unwrap().modified().unwrap()
    );

    let config = temporary.path().join("default.toml");
    let environment = temporary.path().join("snapshot.env");
    std::fs::write(&config, "").unwrap();
    std::fs::write(&environment, "").unwrap();
    let run = |source: &std::path::Path, target: &std::path::Path, captured_at: i64| {
        Command::new(env!("CARGO_BIN_EXE_opencrab-converter"))
            .args(["--source", source.to_str().unwrap()])
            .args(["--target", target.to_str().unwrap()])
            .args(["--config", config.to_str().unwrap()])
            .args(["--environment", environment.to_str().unwrap()])
            .args(["--captured-at", &captured_at.to_string()])
            .output()
            .unwrap()
    };
    let target_a = temporary.path().join("target-a.db");
    let target_b = temporary.path().join("target-b.db");
    let first = run(&source_a, &target_a, FIXTURE_CAPTURED_AT);
    let second = run(&source_b, &target_b, FIXTURE_CAPTURED_AT);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(
        std::fs::read(&target_a).unwrap(),
        std::fs::read(&target_b).unwrap()
    );
    let target = Connection::open(&target_a).unwrap();
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM gate_instance_revisions WHERE created_at=?1",
                [FIXTURE_CAPTURED_AT],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2,
        "all migration-created revisions must use the explicit captured-at"
    );
    assert_eq!(
        target
            .query_row(
                "SELECT COUNT(*) FROM places
                 WHERE close_reason='legacy-history-import' AND closed_at=?1",
                [FIXTURE_CAPTURED_AT],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        5,
        "historical places must use the same explicit migration epoch"
    );
    drop(target);

    let target_c = temporary.path().join("target-c.db");
    let third = run(&source_a, &target_c, FIXTURE_CAPTURED_AT + 1);
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    let first_report: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let third_report: serde_json::Value = serde_json::from_slice(&third.stdout).unwrap();
    assert_ne!(
        first_report["input_snapshot_digest"], third_report["input_snapshot_digest"],
        "the explicit input timestamp must be part of snapshot authority"
    );
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
           (SELECT COUNT(*) FROM legacy_unowned_source_rows)",
        &mut output,
        9,
    );
    output.push_str("SUBJECTS\n");
    append_query(
        &connection,
        "SELECT id,kind,public_id,display_name,printf('%016x',created_at) FROM subjects ORDER BY id",
        &mut output,
        5,
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
        "SELECT p.id,p.public_key,r.classification,r.source_system,r.source_address
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
        "SELECT place_id,seq,kind,author_subject_id,author_external_id,CAST(content AS TEXT)
         FROM events ORDER BY place_id,seq",
        &mut output,
        6,
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
