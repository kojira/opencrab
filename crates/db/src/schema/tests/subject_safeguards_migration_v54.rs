#[test]
fn s2_populated_upgrade_preserves_positive_subject_ids_and_associations_byte_for_byte() {
    let conn = crate::init_memory().expect("init v53 fixture");
    conn.execute_batch(
        "INSERT INTO agents (agent_id, name, persona_name, subject_id)
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
            heartbeat_instructions: String::new(),
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
