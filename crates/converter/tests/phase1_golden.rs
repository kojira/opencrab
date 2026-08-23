use opencrab_converter::{
    convert, ConvertOptions, MigrationInstanceAssembler, MigrationInstanceTarget,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::process::Command;

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
        4
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
    let outcome = convert(
        ConvertOptions {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
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
    let output = Command::new(env!("CARGO_BIN_EXE_opencrab-converter"))
        .args(["--source", source.to_str().unwrap()])
        .args(["--target", target.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("target database path already exists"));
}

fn snapshot(path: &std::path::Path, report: &str) -> String {
    let connection = Connection::open(path).unwrap();
    let mut output = String::new();
    output.push_str("ACCOUNTING\n");
    let report: serde_json::Value = serde_json::from_str(report).unwrap();
    for class in report["classes"].as_array().unwrap() {
        writeln!(
            output,
            "{}|{}|{}|{}|{}|{}|{}",
            class["source_table"].as_str().unwrap(),
            class["logical_class"].as_str().unwrap(),
            class["source_rows"].as_u64().unwrap(),
            class["canonical_outcomes"].as_u64().unwrap(),
            class["raw_outcomes"].as_u64().unwrap(),
            class["dropped_outcomes"].as_u64().unwrap(),
            class["exact_one_violations"].as_u64().unwrap(),
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
