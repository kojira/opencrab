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
        3
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
        5
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
    output.push_str("REPORT\n");
    output.push_str(report);
    output.push_str("SUBJECTS\n");
    append_query(
        &connection,
        "SELECT id,kind,public_id,display_name,created_at FROM subjects ORDER BY id",
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
