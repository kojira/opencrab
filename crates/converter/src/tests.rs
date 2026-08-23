use super::*;

#[test]
fn grant_assembly_uses_closed_roles_unsplit_actions_and_nullable_co_agent_provenance() {
    let source = Connection::open_in_memory().unwrap();
    source
        .execute_batch(
            "CREATE TABLE trusted_users(
               id TEXT PRIMARY KEY,user_id TEXT,agent_id TEXT,permission TEXT,
               created_by TEXT,created_at TEXT,display_name TEXT,platform TEXT
             );
             CREATE TABLE trusted_co_agents(
               id TEXT PRIMARY KEY,agent_id TEXT,co_agent_id TEXT,allowed_actions TEXT,
               created_by TEXT,created_at TEXT
             );
             INSERT INTO trusted_users VALUES
               ('u-a','principal','agent-a','user','fixture','2024-01-01 00:00:00','Principal','discord'),
               ('u-b','principal','agent-a','owner','fixture','2024-01-02 00:00:00','Principal','discord');
             INSERT INTO trusted_co_agents VALUES
               ('c-a','agent-a','agent-b',NULL,'fixture','2024-01-03 00:00:00'),
               ('c-b','agent-a','agent-b','say,react','fixture','2024-01-04 00:00:00');",
        )
        .unwrap();
    let users = SourceTable::load(&source, "trusted_users").unwrap();
    let co_agents = SourceTable::load(&source, "trusted_co_agents").unwrap();
    let mut target = Connection::open_in_memory().unwrap();
    let transaction = target.transaction().unwrap();
    create_phase1_schema(&transaction).unwrap();
    let agents = BTreeMap::from([("agent-a".into(), 1), ("agent-b".into(), 3)]);
    let principals = BTreeMap::from([(("discord".into(), "principal".into()), 2)]);
    let mut raw = RawCollector::default();
    let mut report = ConversionReport::default();

    assemble_grants(
        &transaction,
        &users,
        &co_agents,
        &agents,
        &principals,
        &mut raw,
        &mut report,
    )
    .unwrap();

    assert!(raw.rows.is_empty());
    assert_eq!(
        transaction
            .query_row(
                "SELECT created_at FROM grant_sets WHERE agent_subject_id=1 AND revision=1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        parse_utc_nanos("2024-01-01 00:00:00").unwrap()
    );
    let grants = {
        let mut statement = transaction
            .prepare(
                "SELECT principal_subject_id,role,scope FROM agent_grants
                 ORDER BY principal_subject_id",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        grants,
        vec![
            (2, "owner".into(), "agent".into()),
            (3, "owner_equivalent".into(), "agent".into())
        ]
    );
    assert_eq!(
        transaction
            .query_row("SELECT action FROM grant_actions", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "say,react"
    );
    assert_eq!(
        transaction
            .query_row(
                "SELECT COUNT(*) FROM grant_source_provenance
                 WHERE principal_subject_id=3 AND gate_kind IS NULL AND source_permission IS NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2
    );
    assert_eq!(report.classes[0].canonical_outcomes, 4);
    assert_eq!(report.classes[0].raw_outcomes, 0);
}
