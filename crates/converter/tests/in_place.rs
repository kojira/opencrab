use opencrab_converter::{migrate_in_place, ConverterError, IN_PLACE_MIGRATION_ID};
use opencrab_social_runtime::Policy;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};

const CAPTURED_AT: i64 = 1_770_000_000_000_000_000;
const LLM_LOGS_ADDITIVE_COLUMNS: &[&str] =
    &["turn_record_id", "iteration", "place_id", "subject_id"];
const LLM_LOGS_ADDITIVE_INDEXES: &[&str] = &["idx_llm_logs_turn"];

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
    assert_old_sqlite_master_unchanged(&before, &after);
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
    assert_eq!(new_table_row_digest(&first), new_table_row_digest(&second));
}

#[test]
fn every_migrated_place_policy_json_round_trips_through_policy_from_json() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    Connection::open(&db)
        .unwrap()
        .execute_batch(include_str!("fixtures/phase1-dirty.sql"))
        .unwrap();
    run_migrate(&db);
    let conn = Connection::open(&db).unwrap();
    let mut statement = conn
        .prepare("SELECT id, policy_json FROM places ORDER BY id")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        !rows.is_empty(),
        "fixture must produce at least one migrated place"
    );
    let expected = Policy::default().to_json();
    for (id, policy_json) in rows {
        Policy::from_json(&policy_json).unwrap_or_else(|error| {
            panic!("place {id} policy_json rejected by Policy::from_json: {error}\n{policy_json}")
        });
        assert_eq!(
            policy_json, expected,
            "place {id} must write the oc2 default Policy JSON"
        );
    }
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

#[test]
fn null_personality_stays_canonical_and_writes_null_persona() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    let conn = Connection::open(&db).unwrap();
    opencrab_db::schema::initialize(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO agents(
           agent_id,name,persona_name,personality,instructions,heartbeat_instructions,
           model,created_at,updated_at
         ) VALUES(
           'agent-null-persona','Agent N','persona-n',NULL,'do work','hb',
           'openai:synthetic-model','2024-01-01 00:00:00','2024-01-01 00:00:00'
         );
         INSERT INTO model_pricing(
           provider,model,input_price_per_1m,output_price_per_1m,context_window,updated_at
         ) VALUES('openai','synthetic-model',1,2,128000,'2024-01-01 00:00:00');",
    )
    .unwrap();
    drop(conn);

    run_migrate(&db);
    let conn = Connection::open(&db).unwrap();
    let subjects: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM subjects WHERE kind='agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(subjects, 1);
    let persona: Option<String> = conn
        .query_row("SELECT persona FROM subject_profiles", [], |row| row.get(0))
        .unwrap();
    assert_eq!(persona, None);
    let raw: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM legacy_unowned_source_rows",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw, 0);
}

#[test]
fn missing_discord_config_member_names_absence() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    let conn = Connection::open(&db).unwrap();
    opencrab_db::schema::initialize(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO agents(
           agent_id,name,persona_name,personality,instructions,heartbeat_instructions,
           model,created_at,updated_at
         ) VALUES(
           'agent-present','Agent P','persona-p','kind','do work','hb',
           'openai:synthetic-model','2024-01-01 00:00:00','2024-01-01 00:00:00'
         );
         INSERT INTO model_pricing(
           provider,model,input_price_per_1m,output_price_per_1m,context_window,updated_at
         ) VALUES('openai','synthetic-model',1,2,128000,'2024-01-01 00:00:00');",
    )
    .unwrap();
    drop(conn);

    let config = temporary.path().join("default.toml");
    let environment = temporary.path().join("empty.env");
    std::fs::write(
        &config,
        r#"[gateway.discord]
enabled = true
token = "fixture-token"
agent_ids = ["agent-absent"]
"#,
    )
    .unwrap();
    std::fs::write(&environment, "").unwrap();
    let mut conn = Connection::open(&db).unwrap();
    let error = migrate_in_place(&mut conn, &config, &environment, CAPTURED_AT).unwrap_err();
    match error {
        ConverterError::InstanceSet(message) => {
            assert!(
                message.contains("absent"),
                "instance-set must name absence: {message}"
            );
            assert!(
                !message.contains("exactly once"),
                "instance-set must not say exactly once for absence: {message}"
            );
        }
        other => panic!("expected InstanceSet, got {other}"),
    }
}

#[test]
fn null_personality_discord_shared_member_migrates_end_to_end() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    let conn = Connection::open(&db).unwrap();
    opencrab_db::schema::initialize(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO agents(
           agent_id,name,persona_name,personality,instructions,heartbeat_instructions,
           model,created_at,updated_at
         ) VALUES(
           'agent-null-persona','Agent N','persona-n',NULL,'do work','hb',
           'openai:synthetic-model','2024-01-01 00:00:00','2024-01-01 00:00:00'
         );
         INSERT INTO model_pricing(
           provider,model,input_price_per_1m,output_price_per_1m,context_window,updated_at
         ) VALUES('openai','synthetic-model',1,2,128000,'2024-01-01 00:00:00');",
    )
    .unwrap();
    drop(conn);

    let config = temporary.path().join("default.toml");
    let environment = temporary.path().join("empty.env");
    std::fs::write(
        &config,
        r#"[gateway.discord]
enabled = true
token = "fixture-token"
agent_ids = ["agent-null-persona"]
"#,
    )
    .unwrap();
    std::fs::write(&environment, "").unwrap();
    let mut conn = Connection::open(&db).unwrap();
    migrate_in_place(&mut conn, &config, &environment, CAPTURED_AT).unwrap();

    let subjects: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM subjects WHERE kind='agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(subjects, 1);
    let instances: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM gate_instances WHERE kind_id='discord' AND label='shared:discord'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(instances, 1);
    let raw: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM legacy_unowned_source_rows",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(raw, 0);
    let persona: Option<String> = conn
        .query_row("SELECT persona FROM subject_profiles", [], |row| row.get(0))
        .unwrap();
    assert_eq!(persona, None);
}

#[test]
fn orphan_history_agent_id_lands_in_raw_class_with_byte_identical_ids() {
    const ORPHAN_AGENT_ID: &[u8] = b"1000000000000001";
    let orphan_agent_id = std::str::from_utf8(ORPHAN_AGENT_ID).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    let conn = Connection::open(&db).unwrap();
    opencrab_db::schema::initialize(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO agents(
           agent_id,name,persona_name,personality,instructions,heartbeat_instructions,
           model,created_at,updated_at
         ) VALUES(
           'agent-canonical','Agent C','persona-c','kind','do work','hb',
           'openai:synthetic-model','2024-01-01 00:00:00','2024-01-01 00:00:00'
         );
         INSERT INTO model_pricing(
           provider,model,input_price_per_1m,output_price_per_1m,context_window,updated_at
         ) VALUES('openai','synthetic-model',1,2,128000,'2024-01-01 00:00:00');
         INSERT INTO memory_sessions(
           id,agent_id,session_id,log_type,content,speaker_id,turn_number,metadata_json,created_at
         ) VALUES
           (1,'agent-canonical','sess-canonical','speech','canonical speech','agent-canonical',1,NULL,'2024-01-05 00:00:00'),
           (2,'1000000000000001','sess-orphan-a','speech','orphan speech a','1000000000000001',1,NULL,'2024-01-05 00:01:00'),
           (3,'1000000000000001','sess-orphan-b','speech','orphan speech b','1000000000000001',1,NULL,'2024-01-05 00:02:00'),
           (4,'1000000000000001','sess-orphan-c','system','orphan system drop',NULL,NULL,NULL,'2024-01-05 00:03:00');",
    )
    .unwrap();
    drop(conn);

    let report = run_migrate(&db);
    assert!(
        report
            .classes
            .iter()
            .all(|class| class.exact_one_violations == 0),
        "accounting must balance"
    );

    let history = report
        .classes
        .iter()
        .find(|class| {
            class.source_table == "memory_sessions" && class.logical_class == "history_event"
        })
        .expect("history_event class");
    assert_eq!(history.source_rows, 2);
    assert_eq!(history.canonical_outcomes, 1);
    assert_eq!(history.dropped_outcomes, 1);
    assert_eq!(history.raw_outcomes, 0);

    let orphan = report
        .classes
        .iter()
        .find(|class| {
            class.source_table == "memory_sessions" && class.logical_class == "orphan_history_raw"
        })
        .expect("orphan_history_raw must be a visible report class");
    assert_eq!(orphan.source_rows, 2);
    assert_eq!(orphan.raw_outcomes, 2);
    assert_eq!(orphan.canonical_outcomes, 0);
    assert_eq!(orphan.dropped_outcomes, 0);
    assert_eq!(
        orphan
            .physical_rows
            .get("legacy_unowned_source_rows")
            .copied(),
        Some(2)
    );

    let conn = Connection::open(&db).unwrap();
    let subjects: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM subjects WHERE kind='agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        subjects, 1,
        "canonical agent must migrate; no tombstone subject"
    );
    let orphan_named: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM subjects WHERE name=?1 OR persona=?1",
            [orphan_agent_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(orphan_named, 0, "orphan agent_id must not become a subject");

    let archive: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM legacy_history_archive WHERE source_agent_id=?1",
            [b"agent-canonical".as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(archive, 1);
    let orphan_archive: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM legacy_history_archive WHERE source_agent_id=?1",
            [ORPHAN_AGENT_ID],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(orphan_archive, 0);

    let mut statement = conn
        .prepare(
            "SELECT source_key,row_values,reason FROM legacy_unowned_source_rows
             WHERE source_table='memory_sessions' ORDER BY source_key",
        )
        .unwrap();
    let raw_rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(raw_rows.len(), 2);
    let encoded_id = {
        let mut bytes = vec![3_u8];
        bytes.extend_from_slice(&(ORPHAN_AGENT_ID.len() as u64).to_be_bytes());
        bytes.extend_from_slice(ORPHAN_AGENT_ID);
        bytes
    };
    for (source_key, row_values, reason) in &raw_rows {
        assert_eq!(reason, "history-per-agent-router-v2:orphan_agent_id");
        assert!(
            row_values
                .windows(encoded_id.len())
                .any(|window| window == encoded_id),
            "raw carrier must keep orphan agent_id bytes identical"
        );
        assert!(
            row_values
                .windows(ORPHAN_AGENT_ID.len())
                .any(|window| window == ORPHAN_AGENT_ID),
            "raw row_values must contain the orphan agent_id payload"
        );
        let _ = source_key;
    }
}

#[test]
fn external_speaker_null_metadata_and_turn_number_preserve_nulls() {
    const SPEAKER_ID: &[u8] = b"1000000000000002";
    let speaker_id = std::str::from_utf8(SPEAKER_ID).unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    let conn = Connection::open(&db).unwrap();
    opencrab_db::schema::initialize(&conn).unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO agents(
           agent_id,name,persona_name,personality,instructions,heartbeat_instructions,
           model,created_at,updated_at
         ) VALUES(
           'agent-canonical','Agent C','persona-c','kind','do work','hb',
           'openai:synthetic-model','2024-01-01 00:00:00','2024-01-01 00:00:00'
         );
         INSERT INTO model_pricing(
           provider,model,input_price_per_1m,output_price_per_1m,context_window,updated_at
         ) VALUES('openai','synthetic-model',1,2,128000,'2024-01-01 00:00:00');
         INSERT INTO memory_sessions(
           id,agent_id,session_id,log_type,content,speaker_id,turn_number,metadata_json,created_at
         ) VALUES
           (1,'agent-canonical','sess-external','speech','external speech','{speaker_id}',NULL,NULL,'2024-01-05 00:00:00');"
    ))
    .unwrap();
    drop(conn);

    let report = run_migrate(&db);
    assert!(
        report
            .classes
            .iter()
            .all(|class| class.exact_one_violations == 0),
        "accounting must balance"
    );

    let history = report
        .classes
        .iter()
        .find(|class| {
            class.source_table == "memory_sessions" && class.logical_class == "history_event"
        })
        .expect("history_event class");
    assert_eq!(history.source_rows, 1);
    assert_eq!(history.canonical_outcomes, 1);
    assert_eq!(history.dropped_outcomes, 0);
    assert_eq!(history.raw_outcomes, 0);
    assert_eq!(
        history.physical_rows.get("legacy_history_archive").copied(),
        Some(1)
    );
    assert!(
        report
            .classes
            .iter()
            .all(|class| class.logical_class != "orphan_history_raw"),
        "canonical external speaker must not take the orphan raw class"
    );

    let conn = Connection::open(&db).unwrap();
    let archive = conn
        .query_row(
            "SELECT speaker_source_id,source_turn_number,metadata,log_kind
             FROM legacy_history_archive",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<Vec<u8>>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(archive.0.as_deref(), Some(SPEAKER_ID));
    assert_eq!(archive.1, None, "NULL turn_number must stay NULL");
    assert_eq!(archive.2, None, "NULL metadata_json must stay NULL");
    assert_eq!(archive.3, "speech");

    let said: i64 = conn
        .query_row("SELECT COUNT(*) FROM events WHERE kind='said'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        said, 0,
        "NULL metadata cannot invent a Discord/Nostr Said join"
    );

    let audits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM legacy_audit_records WHERE metadata IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audits, 1);
}

#[test]
fn malformed_non_null_history_metadata_fails_loud() {
    let temporary = tempfile::tempdir().unwrap();
    let db = temporary.path().join("source.db");
    let conn = Connection::open(&db).unwrap();
    opencrab_db::schema::initialize(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO agents(
           agent_id,name,persona_name,personality,instructions,heartbeat_instructions,
           model,created_at,updated_at
         ) VALUES(
           'agent-canonical','Agent C','persona-c','kind','do work','hb',
           'openai:synthetic-model','2024-01-01 00:00:00','2024-01-01 00:00:00'
         );
         INSERT INTO model_pricing(
           provider,model,input_price_per_1m,output_price_per_1m,context_window,updated_at
         ) VALUES('openai','synthetic-model',1,2,128000,'2024-01-01 00:00:00');
         INSERT INTO memory_sessions(
           id,agent_id,session_id,log_type,content,speaker_id,turn_number,metadata_json,created_at
         ) VALUES
           (1,'agent-canonical','sess-external','speech','external speech','1000000000000002',NULL,'{','2024-01-05 00:00:00');",
    )
    .unwrap();
    drop(conn);

    let (config, environment) = write_inputs(db.parent().unwrap());
    let mut conn = Connection::open(&db).unwrap();
    let error = migrate_in_place(&mut conn, &config, &environment, CAPTURED_AT).unwrap_err();
    match error {
        ConverterError::Accounting(message) => {
            assert!(
                message.contains("malformed inspected metadata"),
                "expected malformed inspect failure, got {message}"
            );
        }
        other => panic!("expected Accounting, got {other}"),
    }
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

struct MasterEntry {
    type_name: String,
    name: String,
    tbl_name: String,
    sql: Option<String>,
}

struct LegacySnapshot {
    names: Vec<String>,
    counts: BTreeMap<String, i64>,
    columns: BTreeMap<String, Vec<String>>,
    master: Vec<MasterEntry>,
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
        master: capture_sqlite_master(&conn),
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

fn capture_sqlite_master(conn: &Connection) -> Vec<MasterEntry> {
    let mut statement = conn
        .prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%'
             ORDER BY type, name",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok(MasterEntry {
                type_name: row.get(0)?,
                name: row.get(1)?,
                tbl_name: row.get(2)?,
                sql: row.get(3)?,
            })
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
}

fn assert_old_sqlite_master_unchanged(before: &LegacySnapshot, after: &LegacySnapshot) {
    let old_tables: BTreeSet<&str> = before.names.iter().map(String::as_str).collect();
    let allowed_new_indexes: BTreeSet<&str> = LLM_LOGS_ADDITIVE_INDEXES.iter().copied().collect();
    let before_by_key: BTreeMap<(&str, &str), &MasterEntry> = before
        .master
        .iter()
        .map(|entry| ((entry.type_name.as_str(), entry.name.as_str()), entry))
        .collect();
    let after_old: Vec<&MasterEntry> = after
        .master
        .iter()
        .filter(|entry| old_tables.contains(entry.tbl_name.as_str()))
        .collect();

    for entry in &before.master {
        let after_entry = after_old
            .iter()
            .find(|candidate| {
                candidate.type_name == entry.type_name && candidate.name == entry.name
            })
            .unwrap_or_else(|| {
                panic!(
                    "old sqlite_master entry vanished: {} {}",
                    entry.type_name, entry.name
                )
            });
        if entry.tbl_name == "llm_logs" && entry.type_name == "table" {
            assert_eq!(
                strip_enumerated_llm_logs_columns(after_entry.sql.as_deref().unwrap_or("")),
                entry.sql.as_deref().unwrap_or(""),
                "llm_logs sqlite_master sql must match modulo enumerated ADD COLUMN"
            );
        } else {
            assert_eq!(
                after_entry.sql, entry.sql,
                "old sqlite_master {}.{} sql must be unchanged",
                entry.type_name, entry.name
            );
        }
    }

    for entry in after_old {
        if before_by_key.contains_key(&(entry.type_name.as_str(), entry.name.as_str())) {
            continue;
        }
        assert!(
            entry.tbl_name == "llm_logs"
                && entry.type_name == "index"
                && allowed_new_indexes.contains(entry.name.as_str()),
            "unexpected sqlite_master addition on old table {}: {} {}",
            entry.tbl_name,
            entry.type_name,
            entry.name
        );
    }
}

fn strip_enumerated_llm_logs_columns(sql: &str) -> String {
    let mut out = sql.to_string();
    for column in LLM_LOGS_ADDITIVE_COLUMNS {
        for pattern in [
            format!(", {column} INTEGER"),
            format!(",{column} INTEGER"),
            format!(",\n              {column} INTEGER"),
        ] {
            out = out.replace(&pattern, "");
        }
    }
    out
}

fn new_table_row_digest(path: &std::path::Path) -> String {
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
        let columns = table_columns(&conn, &name);
        let quoted: Vec<String> = columns
            .iter()
            .map(|column| format!("\"{column}\""))
            .collect();
        digest.push_str(&name);
        digest.push('\n');
        if quoted.is_empty() {
            continue;
        }
        let order = quoted.join(",");
        let sql = format!("SELECT {order} FROM \"{name}\" ORDER BY {order}");
        let mut rows = conn.prepare(&sql).unwrap();
        let mut query = rows.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            for index in 0..columns.len() {
                if index > 0 {
                    digest.push('|');
                }
                digest.push_str(&value_digest(row.get_ref(index).unwrap()));
            }
            digest.push('\n');
        }
    }
    digest
}

fn value_digest(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(value) => value.to_string(),
        ValueRef::Real(value) => format!("{value:?}"),
        ValueRef::Text(value) => format!("T:{}", String::from_utf8_lossy(value)),
        ValueRef::Blob(value) => {
            let mut hex = String::from("B:");
            for byte in value {
                hex.push_str(&format!("{byte:02x}"));
            }
            hex
        }
    }
}
