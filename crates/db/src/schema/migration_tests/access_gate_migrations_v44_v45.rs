use super::super::*;
use super::support::user_tables;
use rusqlite::Connection;
fn setup_pre_v44(conn: &Connection) {
    conn.execute_batch(
        "DROP TABLE IF EXISTS deliveries;
         DROP TABLE IF EXISTS external_origins;
         DROP TABLE IF EXISTS gate_bindings;
         DROP TABLE IF EXISTS gate_instances;
         DROP TRIGGER IF EXISTS agents_subject_id_insert_guard;
         DROP TRIGGER IF EXISTS agents_subject_id_assign;
         DROP TRIGGER IF EXISTS agents_subject_id_update_guard;
         DROP INDEX IF EXISTS idx_agents_subject_id;
         ALTER TABLE agents DROP COLUMN subject_id;
         PRAGMA user_version = 43;",
    )
    .unwrap();
}

fn gate_user_tables(conn: &Connection) -> Vec<String> {
    user_tables(conn)
        .into_iter()
        .filter(|n| {
            matches!(
                n.as_str(),
                "gate_instances" | "gate_bindings" | "external_origins" | "deliveries"
            )
        })
        .collect()
}

fn assert_v44_schema(conn: &Connection) {
    assert!(column_exists(conn, "agents", "subject_id").unwrap());
    assert_eq!(
        gate_user_tables(conn),
        vec![
            "deliveries".to_string(),
            "external_origins".to_string(),
            "gate_bindings".to_string(),
            "gate_instances".to_string(),
        ]
    );
    let idx: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='index' AND name='idx_agents_subject_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 1);
    let open_addr: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='index' AND name='idx_gate_bindings_open_address'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(open_addr, 1);
    for trigger in [
        "agents_subject_id_insert_guard",
        "agents_subject_id_assign",
        "agents_subject_id_update_guard",
    ] {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name=?1",
                [trigger],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "trigger {trigger} が無い");
    }
}

/// §9.6: fresh DB と user_version 43 相当 DB の双方が v44 へ到達し、gate 表は exact 4 表。
#[test]
fn v44_fresh_and_user_version_43_reach_four_gate_tables() {
    let fresh = crate::init_memory().expect("fresh");
    assert_eq!(schema_version(&fresh).unwrap(), latest_version());
    assert_v44_schema(&fresh);

    let from_43 = crate::init_memory().expect("from43");
    setup_pre_v44(&from_43);
    assert_eq!(schema_version(&from_43).unwrap(), 43);
    assert!(!column_exists(&from_43, "agents", "subject_id").unwrap());
    assert!(gate_user_tables(&from_43).is_empty());

    initialize(&from_43).expect("v44 from 43");
    assert_eq!(schema_version(&from_43).unwrap(), latest_version());
    assert_v44_schema(&from_43);
}

/// §9.6: subject backfill / 自動採番 / unique / positive guard。
#[test]
fn v44_agents_subject_id_backfill_assign_and_guards() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v44(&conn);
    conn.execute_batch(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES
            ('z-agent', 'Z', 'pz'),
            ('a-agent', 'A', 'pa');",
    )
    .unwrap();
    initialize(&conn).expect("v44");

    let rows: Vec<(String, i64)> = conn
        .prepare("SELECT agent_id, subject_id FROM agents ORDER BY subject_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![("a-agent".to_string(), 1), ("z-agent".to_string(), 2),]
    );

    conn.execute(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES ('n-agent', 'N', 'pn')",
        [],
    )
    .unwrap();
    let assigned: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id='n-agent'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(assigned, 3);

    let non_positive = conn.execute(
        "INSERT INTO agents (agent_id, name, persona_name, subject_id)
         VALUES ('bad', 'B', 'pb', 0)",
        [],
    );
    assert!(non_positive.is_err(), "subject_id=0 を受け入れた");

    let update_null = conn.execute(
        "UPDATE agents SET subject_id = NULL WHERE agent_id='a-agent'",
        [],
    );
    assert!(update_null.is_err(), "subject_id NULL 更新を受け入れた");

    let dup = conn.execute(
        "UPDATE agents SET subject_id = 1 WHERE agent_id='z-agent'",
        [],
    );
    assert!(dup.is_err(), "subject_id unique が効いていない");
}

/// §9.6: 4 表の PK/FK/CHECK/partial unique。
#[test]
fn v44_gate_table_constraints() {
    let conn = crate::init_memory().expect("init");
    conn.execute(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES ('ag', 'A', 'p')",
        [],
    )
    .unwrap();
    let subject: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id='ag'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    let digest = "a".repeat(64);
    conn.execute(
        "INSERT INTO gate_instances (
            instance_id, kind_id, subject_id, revision, enabled,
            config_b64, config_digest, created_at, updated_at
         ) VALUES ('inst-1', 'k', ?1, 1, 1, 'e30=', ?2, 1, 1)",
        rusqlite::params![subject, digest],
    )
    .unwrap();

    let empty_kind = conn.execute(
        "INSERT INTO gate_instances (
            instance_id, kind_id, subject_id, revision, enabled,
            config_b64, config_digest, created_at, updated_at
         ) VALUES ('inst-2', '', ?1, 1, 1, 'e30=', ?2, 1, 1)",
        rusqlite::params![subject, digest],
    );
    assert!(empty_kind.is_err(), "空 kind_id を受け入れた");

    conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at)
         VALUES ('bind-1', 'inst-1', 'addr-a', 1)",
        [],
    )
    .unwrap();
    let dup_open = conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at)
         VALUES ('bind-2', 'inst-1', 'addr-a', 2)",
        [],
    );
    assert!(dup_open.is_err(), "open address unique が効いていない");

    conn.execute(
        "UPDATE gate_bindings SET closed_at = 3 WHERE binding_id='bind-1'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at)
         VALUES ('bind-2', 'inst-1', 'addr-a', 4)",
        [],
    )
    .expect("closed 後の同 address を拒否した");

    conn.execute(
        "INSERT INTO external_origins (binding_id, origin, seq) VALUES ('bind-2', 'o1', 1)",
        [],
    )
    .unwrap();
    let dup_origin = conn.execute(
        "INSERT INTO external_origins (binding_id, origin, seq) VALUES ('bind-2', 'o1', 2)",
        [],
    );
    assert!(dup_origin.is_err(), "origin PK が効いていない");
    let dup_seq = conn.execute(
        "INSERT INTO external_origins (binding_id, origin, seq) VALUES ('bind-2', 'o2', 1)",
        [],
    );
    assert!(dup_seq.is_err(), "seq unique が効いていない");

    let bad_payload = conn.execute(
        "INSERT INTO deliveries (
            delivery_id, binding_id, payload_json, state, created_at, updated_at
         ) VALUES ('d1', 'bind-2', '{\"text\":\"\"}', 'sending', 1, 1)",
        [],
    );
    assert!(bad_payload.is_err(), "空 text payload を受け入れた");

    conn.execute(
        "INSERT INTO deliveries (
            delivery_id, binding_id, payload_json, state, created_at, updated_at
         ) VALUES ('d1', 'bind-2', '{\"text\":\"hi\"}', 'sending', 1, 1)",
        [],
    )
    .unwrap();
    let bad_failed = conn.execute(
        "UPDATE deliveries SET state='failed', error='disconnect' WHERE delivery_id='d1'",
        [],
    );
    assert!(bad_failed.is_err(), "failed に disconnect を受け入れた");
}

/// §9.6: migration 中の statement 失敗は全 rollback、user_version は 43 のまま。
#[test]
fn v44_statement_failure_rolls_back_and_keeps_user_version_43() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v44(&conn);
    conn.execute_batch("CREATE INDEX gate_instances ON agents(agent_id);")
        .unwrap();
    assert_eq!(schema_version(&conn).unwrap(), 43);

    let err = initialize(&conn);
    assert!(err.is_err(), "衝突 index があっても v44 が成功した");
    assert_eq!(schema_version(&conn).unwrap(), 43);
    assert!(
        !column_exists(&conn, "agents", "subject_id").unwrap(),
        "失敗したのに subject_id が残った"
    );
    assert!(
        gate_user_tables(&conn).is_empty(),
        "失敗したのに gate 表が残った: {:?}",
        gate_user_tables(&conn)
    );
}

fn setup_pre_v45(conn: &Connection) {
    conn.execute_batch(
        "DROP TABLE IF EXISTS nostr_bundle_state;
         PRAGMA user_version = 44;",
    )
    .unwrap();
}

fn assert_v45_schema(conn: &Connection) {
    assert_v44_schema(conn);
    assert!(
        table_exists(conn, "nostr_bundle_state").unwrap(),
        "nostr_bundle_state が無い"
    );
    assert_eq!(
        gate_user_tables(conn),
        vec![
            "deliveries".to_string(),
            "external_origins".to_string(),
            "gate_bindings".to_string(),
            "gate_instances".to_string(),
        ],
        "V3 4表の集合が変わった"
    );
}

#[test]
fn v45_fresh_and_user_version_44_reach_bundle_state() {
    let fresh = crate::init_memory().expect("fresh");
    assert_eq!(schema_version(&fresh).unwrap(), latest_version());
    assert_v45_schema(&fresh);

    let from_44 = crate::init_memory().expect("from44");
    setup_pre_v45(&from_44);
    assert_eq!(schema_version(&from_44).unwrap(), 44);
    assert!(!table_exists(&from_44, "nostr_bundle_state").unwrap());

    initialize(&from_44).expect("v45 from 44");
    assert_eq!(schema_version(&from_44).unwrap(), latest_version());
    assert_v45_schema(&from_44);
}

#[test]
fn v45_rerun_is_noop() {
    let conn = crate::init_memory().expect("init");
    setup_pre_v45(&conn);
    initialize(&conn).expect("v45");
    initialize(&conn).expect("v45 rerun");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert_v45_schema(&conn);
}

#[test]
fn v45_bundle_state_pk_and_completed_check() {
    let conn = crate::init_memory().expect("init");
    conn.execute(
        "INSERT INTO nostr_bundle_state
            (binding_id, bundle_id, manifest_json, received_bits, new_admitted_bits, completed)
         VALUES ('b1', 'id1', '[\"o1\"]', '0', '0', 0)",
        [],
    )
    .unwrap();
    let dup = conn.execute(
        "INSERT INTO nostr_bundle_state
            (binding_id, bundle_id, manifest_json, received_bits, new_admitted_bits, completed)
         VALUES ('b1', 'id1', '[\"o2\"]', '1', '1', 1)",
        [],
    );
    assert!(dup.is_err(), "PRIMARY KEY が効いていない");
    let bad = conn.execute(
        "INSERT INTO nostr_bundle_state
            (binding_id, bundle_id, manifest_json, received_bits, new_admitted_bits, completed)
         VALUES ('b1', 'id2', '[\"o1\"]', '0', '0', 2)",
        [],
    );
    assert!(bad.is_err(), "completed 2 を受け入れた");
}
