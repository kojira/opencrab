use super::*;

#[test]
fn inspected_json_rejects_duplicate_keys_at_every_depth() {
    assert!(parse_json_without_duplicate_keys(br#"{"outer":{"key":1,"key":2}}"#).is_err());
    assert!(parse_json_without_duplicate_keys(br#"{"key":1,"key":2}"#).is_err());
    assert_eq!(
        parse_json_without_duplicate_keys(br#"{"outer":{"key":1},"items":[true,null]}"#).unwrap()
            ["outer"]["key"],
        1
    );
}

#[test]
fn discord_policy_anchors_guild_routes_from_subject_policy() {
    let mut connection = Connection::open_in_memory().unwrap();
    let transaction = connection.transaction().unwrap();
    create_phase1_schema(&transaction).unwrap();
    transaction
        .execute_batch(
            r#"INSERT INTO subjects(id,kind,name,persona,turn_runner,standing,created_at)
               VALUES(1,'agent','Agent','Agent','engine','trusted',0);
             INSERT INTO places(id,address,parent_id,policy_json,inherit_from_place,inherit_up_to_seq,created_at,closed_at,close_reason)
               VALUES(1,NULL,NULL,'hard-default',NULL,NULL,0,NULL,NULL);
             INSERT INTO memberships VALUES(1,1,'participant',0,0);
             INSERT INTO gate_instances VALUES(
               '11111111-1111-4111-8111-111111111111','discord','shared',NULL,1,'stopped'
             );
             INSERT INTO gate_instance_revisions(
               instance_id,revision,present,enabled,config_schema_id,config_bytes,
               config_digest,secret_set_id,created_at
             ) VALUES(
               '11111111-1111-4111-8111-111111111111',1,1,1,'fixture',x'00',
               zeroblob(32),NULL,0
             );
             INSERT INTO gate_bindings(
               binding_id,place_id,instance_id,address,label,origin_scope_id,
               binding_metadata_schema_id,binding_metadata_bytes,binding_metadata_digest
             ) VALUES(
               '22222222-2222-4222-8222-222222222222',1,
               '11111111-1111-4111-8111-111111111111','111',NULL,'scope',
               'gate-binding/discord/v1',CAST('{"address_kind":"guild"}' AS BLOB),
               zeroblob(32)
             );
             INSERT INTO place_subject_policies(
               place_id,kind_id,subject_id,admission,readable,writable,whitelisted,
               heartbeat_enabled,heartbeat_interval_secs,heartbeat_instructions,
               instructions_revision,source_row,source_updated_at
             ) VALUES(
               1,'discord',1,'open',1,1,1,0,NULL,'',1,x'00',0
             );"#,
        )
        .unwrap();

    reconcile_migrated_routes(&transaction).unwrap();

    assert_eq!(
        transaction
            .query_row("SELECT COUNT(*) FROM subject_routes", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

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
    let mut raw = RawCollector::new(&transaction);
    let mut report = ConversionReport::default();
    let provenance = MigrationProvenance::new([7; 32]);

    assemble_grants(
        &transaction,
        &users,
        &co_agents,
        &agents,
        &principals,
        &provenance,
        &mut raw,
        &mut report,
    )
    .unwrap();

    assert_eq!(
        transaction
            .query_row(
                "SELECT COUNT(*) FROM legacy_unowned_source_rows",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .unwrap(),
        0
    );
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
                 WHERE principal_subject_id=3 AND gate_kind='all' AND source_permission='co-agent'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2
    );
    assert_eq!(report.classes[0].canonical_outcomes, 4);
    assert_eq!(report.classes[0].raw_outcomes, 0);
    assert_eq!(
        transaction
            .query_row("SELECT COUNT(*) FROM migration_provenance", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        13
    );
    for (table, source_table) in [(&users, "trusted_users"), (&co_agents, "trusted_co_agents")] {
        for row in &table.rows {
            let found = transaction
                .query_row(
                    "SELECT COUNT(*) FROM migration_provenance
                     WHERE target_entity='grant_source_provenance'
                       AND source_locator=?1 AND source_key=?2 AND source_row_digest=?3",
                    params![
                        format!("table:{source_table}"),
                        row.source_key,
                        row.row_digest.as_slice()
                    ],
                    |result| result.get::<_, i64>(0),
                )
                .unwrap();
            assert_eq!(found, 1);
        }
    }
}

#[test]
fn principal_assembly_raw_routes_empty_external_id_and_preserves_all_contributors() {
    let source = Connection::open_in_memory().unwrap();
    source
        .execute_batch(
            "CREATE TABLE trusted_users(
               id TEXT PRIMARY KEY,user_id TEXT,agent_id TEXT,permission TEXT,
               created_by TEXT,created_at TEXT,display_name TEXT,platform TEXT
             );
             INSERT INTO trusted_users VALUES
               ('u-a','principal','agent-a','user','fixture','2024-01-01 00:00:00','Winner','discord'),
               ('u-b','principal','agent-a','owner','fixture','2024-01-02 00:00:00','Other','discord'),
               ('u-empty','','agent-a','user','fixture','2024-01-03 00:00:00','Empty Id','discord');",
        )
        .unwrap();
    let users = SourceTable::load(&source, "trusted_users").unwrap();
    let mut target = Connection::open_in_memory().unwrap();
    let transaction = target.transaction().unwrap();
    create_phase1_schema(&transaction).unwrap();
    transaction
        .execute_batch(
            "INSERT INTO gate_instances(
               instance_id,kind_id,label,owner_subject_id,active_revision,lifecycle
             ) VALUES
               ('22222222-2222-4222-8222-222222222222','discord','fixture-b',NULL,1,'stopped'),
               ('11111111-1111-4111-8111-111111111111','discord','fixture-a',NULL,1,'stopped')",
        )
        .unwrap();
    let instances = load_migration_instances(&transaction).unwrap();
    let provenance = MigrationProvenance::new([9; 32]);
    let mut raw = RawCollector::new(&transaction);
    let mut report = ConversionReport::default();

    let principals = assemble_principals(
        &transaction,
        &users,
        &instances,
        &provenance,
        &mut raw,
        &mut report,
        0,
    )
    .unwrap();
    report.verify().unwrap();

    assert_eq!(principals.len(), 1);
    assert_eq!(
        principals[0].instances,
        [
            "11111111-1111-4111-8111-111111111111",
            "22222222-2222-4222-8222-222222222222"
        ]
    );
    assert_eq!(report.classes[0].canonical_outcomes, 2);
    assert_eq!(report.classes[0].raw_outcomes, 1);
    assert_eq!(report.classes[0].exact_one_violations, 0);
    let empty = users
        .rows
        .iter()
        .find(|row| users.text(row, "user_id") == Some(""))
        .unwrap();
    assert_eq!(
        transaction
            .query_row(
                "SELECT COUNT(*) FROM legacy_unowned_source_rows
                 WHERE source_table=?1 AND source_key=?2",
                params![users.name, empty.source_key],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    assert_eq!(
        transaction
            .query_row(
                "SELECT COUNT(*) FROM migration_provenance
                 WHERE target_entity='subjects' AND source_locator='table:trusted_users'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2
    );
    for row in users
        .rows
        .iter()
        .filter(|row| users.text(row, "user_id") == Some("principal"))
    {
        let found = transaction
            .query_row(
                "SELECT COUNT(*) FROM migration_provenance
                 WHERE target_entity='subjects' AND source_key=?1 AND source_row_digest=?2",
                params![row.source_key, row.row_digest.as_slice()],
                |result| result.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(found, 1);
    }
}
