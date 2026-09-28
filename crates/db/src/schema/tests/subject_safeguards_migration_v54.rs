#[test]
fn s2_populated_upgrade_preserves_positive_subject_ids_and_associations_byte_for_byte() {
    let conn = crate::init_memory().expect("init v53 fixture");
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS subject_allocator_no_delete;
         DROP TRIGGER IF EXISTS subject_allocator_monotonic;
         DROP TRIGGER IF EXISTS subject_tombstones_no_update;
         DROP TRIGGER IF EXISTS subject_tombstones_no_delete;
         DROP TRIGGER IF EXISTS subject_grants_no_delete;
         DROP TRIGGER IF EXISTS subject_grants_consume_once;
         DROP TRIGGER IF EXISTS agents_subject_id_insert_guard;
         DROP TRIGGER IF EXISTS agents_subject_id_assign;
         DROP TRIGGER IF EXISTS agents_subject_id_advance_explicit;
         DROP TRIGGER IF EXISTS agents_subject_id_update_guard;
         DROP TRIGGER IF EXISTS agents_subject_tombstone_delete_guard;
         DROP TABLE subject_association_grants;
         DROP TABLE subject_tombstones;
         DROP TABLE subject_id_allocator;
         ALTER TABLE gate_bindings DROP COLUMN session_id;
         ALTER TABLE gate_instances DROP COLUMN association_grandfathered;
         INSERT INTO agents (agent_id, name, persona_name, subject_id)
             VALUES ('s2-agent-a', 'A', 'p', 41), ('s2-agent-b', 'B', 'p', 97);
         INSERT INTO gate_instances
             (instance_id, kind_id, subject_id, revision, enabled, config_b64,
              config_digest, created_at, updated_at)
             VALUES
             ('00000000-0000-4000-8000-000000000041', 'opaque-a', 41, 1, 1,
              'YQ==', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 1, 1),
             ('00000000-0000-4000-8000-000000000097', 'opaque-b', 97, 1, 0,
              'Yg==', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 2, 2);
         PRAGMA user_version = 53;",
    )
    .unwrap();
    let before: Vec<(String, String)> = conn
        .prepare(
            "SELECT agent_id, CAST(subject_id AS TEXT) FROM agents
             WHERE agent_id LIKE 's2-agent-%' ORDER BY agent_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let associations_before: Vec<(String, String)> = conn
        .prepare(
            "SELECT instance_id, CAST(subject_id AS TEXT) FROM gate_instances
             WHERE instance_id LIKE '00000000-0000-4000-8000-%' ORDER BY instance_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();

    initialize(&conn).expect("apply S2 migration");

    assert_eq!(
        conn.query_row(
            "SELECT next_subject_id FROM subject_id_allocator WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("S2 subject allocator must exist"),
        98
    );
    let after: Vec<(String, String)> = conn
        .prepare(
            "SELECT agent_id, CAST(subject_id AS TEXT) FROM agents
             WHERE agent_id LIKE 's2-agent-%' ORDER BY agent_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let associations_after: Vec<(String, String)> = conn
        .prepare(
            "SELECT instance_id, CAST(subject_id AS TEXT) FROM gate_instances
             WHERE instance_id LIKE '00000000-0000-4000-8000-%' ORDER BY instance_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(after, before, "subject IDs changed during S2 upgrade");
    assert_eq!(
        associations_after, associations_before,
        "gate associations changed during S2 upgrade"
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM gate_instances WHERE association_grandfathered = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        2,
        "pre-S2 associations were not grandfathered"
    );
}

#[test]
fn s2_pre_schema_snapshot_restores_byte_identical_fixture() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("core.sqlite");
    let snapshot = directory.path().join("core-v53.snapshot");
    {
        let conn = crate::init_connection(database.to_str().unwrap()).unwrap();
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS subject_allocator_no_delete;
             DROP TRIGGER IF EXISTS subject_allocator_monotonic;
             DROP TRIGGER IF EXISTS subject_tombstones_no_update;
             DROP TRIGGER IF EXISTS subject_tombstones_no_delete;
             DROP TRIGGER IF EXISTS subject_grants_no_delete;
             DROP TRIGGER IF EXISTS subject_grants_consume_once;
             DROP TRIGGER IF EXISTS agents_subject_id_insert_guard;
             DROP TRIGGER IF EXISTS agents_subject_id_assign;
             DROP TRIGGER IF EXISTS agents_subject_id_advance_explicit;
             DROP TRIGGER IF EXISTS agents_subject_id_update_guard;
             DROP TRIGGER IF EXISTS agents_subject_tombstone_delete_guard;
             DROP TABLE subject_association_grants;
             DROP TABLE subject_tombstones;
             DROP TABLE subject_id_allocator;
             ALTER TABLE gate_bindings DROP COLUMN session_id;
             ALTER TABLE gate_instances DROP COLUMN association_grandfathered;
             INSERT INTO agents (agent_id, name, persona_name, subject_id)
                 VALUES ('rollback-agent', 'Rollback', 'p', 71);
             INSERT INTO gate_instances
                 (instance_id, kind_id, subject_id, revision, enabled, config_b64,
                  config_digest, created_at, updated_at)
                 VALUES ('00000000-0000-4000-8000-000000000071', 'opaque', 71, 1, 1,
                         'e30=', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 1, 1);
             PRAGMA user_version = 53;
             PRAGMA wal_checkpoint(TRUNCATE);
             PRAGMA journal_mode=DELETE;",
        )
        .unwrap();
    }
    std::fs::copy(&database, &snapshot).unwrap();
    let snapshot_bytes = std::fs::read(&snapshot).unwrap();

    {
        let conn = crate::init_connection(database.to_str().unwrap()).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), latest_version());
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
            .unwrap();
    }

    std::fs::copy(&snapshot, &database).unwrap();
    assert_eq!(
        std::fs::read(&database).unwrap(),
        snapshot_bytes,
        "restored pre-S2 snapshot differs byte-for-byte"
    );
    let restored = Connection::open(&database).unwrap();
    assert_eq!(schema_version(&restored).unwrap(), 53);
    assert_eq!(
        restored
            .query_row(
                "SELECT subject_id FROM agents WHERE agent_id='rollback-agent'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        71
    );
    assert_eq!(
        restored
            .query_row(
                "SELECT subject_id FROM gate_instances
                 WHERE instance_id='00000000-0000-4000-8000-000000000071'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        71
    );
}

#[test]
fn s2_fresh_schema_installs_allocator_tombstones_and_hashed_grants() {
    let conn = crate::init_memory().expect("fresh schema");
    for table in [
        "subject_id_allocator",
        "subject_tombstones",
        "subject_association_grants",
    ] {
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "missing S2 table {table}");
    }
    let grant_columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('subject_association_grants') ORDER BY cid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(grant_columns.contains(&"grant_hash".to_string()));
    assert!(
        !grant_columns.iter().any(|name| name.contains("token")),
        "plaintext grant token column is forbidden: {grant_columns:?}"
    );
}

#[test]
fn s2_allocator_rejects_decrement_reset_and_delete_and_preserves_high_water() {
    let conn = crate::init_memory().unwrap();
    conn.execute_batch(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES
             ('s2-high-water-a', 'a', 'p'),
             ('s2-high-water-b', 'b', 'p'),
             ('s2-high-water-c', 'c', 'p');",
    )
    .unwrap();
    let prior_next: i64 = conn
        .query_row(
            "SELECT next_subject_id FROM subject_id_allocator WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(prior_next > 3);

    for (name, sql) in [
        (
            "decrement",
            "UPDATE subject_id_allocator SET next_subject_id=next_subject_id-1 WHERE singleton=1",
        ),
        (
            "reset",
            "UPDATE subject_id_allocator SET next_subject_id=1 WHERE singleton=1",
        ),
        (
            "delete",
            "DELETE FROM subject_id_allocator WHERE singleton=1",
        ),
    ] {
        assert!(
            conn.execute_batch(sql).is_err(),
            "allocator {name} unexpectedly succeeded"
        );
        assert_eq!(
            conn.query_row(
                "SELECT next_subject_id FROM subject_id_allocator WHERE singleton=1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            prior_next,
            "allocator {name} changed the high-water record"
        );
    }

    conn.execute(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES ('s2-after-high-water', 'd', 'p')",
        [],
    )
    .unwrap();
    let allocated: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id='s2-after-high-water'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(allocated, prior_next);
    assert!(allocated > prior_next - 1, "allocator reused its prior high-water");
}

#[test]
fn s2_hard_delete_tombstones_subject_and_allocator_never_reuses_it() {
    use crate::queries::{delete_agent, upsert_agent, AgentRow};

    fn agent(id: &str) -> AgentRow {
        AgentRow {
            agent_id: id.into(),
            name: id.into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "p".into(),
            personality: None,
            instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        }
    }

    let conn = crate::init_memory().unwrap();
    upsert_agent(&conn, &agent("s2-deleted")).unwrap();
    let deleted_subject: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id='s2-deleted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(delete_agent(&conn, "s2-deleted").unwrap());
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM subject_tombstones WHERE subject_id=?1",
            [deleted_subject],
            |row| row.get::<_, i64>(0),
        )
        .expect("hard delete must create a permanent tombstone"),
        1
    );

    upsert_agent(&conn, &agent("s2-successor")).unwrap();
    let successor: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id='s2-successor'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(successor > deleted_subject, "subject ID was reused");
    assert!(conn
        .execute(
            "DELETE FROM subject_tombstones WHERE subject_id=?1",
            [deleted_subject],
        )
        .is_err(), "subject tombstones must be permanent");
}
