#[test]
fn s8_migrates_existing_nostr_gateway_store_without_losing_identity_or_watch() {
    let temp = tempfile::tempdir().unwrap();
    let core_path = temp.path().join("core.db");
    let gateway_path = temp.path().join("nostr.db");
    let key_path = temp.path().join("nostr.key");
    let approval_path = temp.path().join("approval.json");
    let report_path = temp.path().join("report.json");
    let backup_dir = temp.path().join("backup");
    seed_core(&core_path);
    let core = Connection::open(&core_path).unwrap();
    core.execute_batch(
        "DELETE FROM agent_discord_config; DELETE FROM channel_config; DELETE FROM trusted_users;",
    )
    .unwrap();
    let config = serde_json::json!({
        "relays":["wss://example.invalid"], "filter":{"kinds":[1]},
        "self_pubkey":"a".repeat(64), "name":"A",
        "access":{"owner":["b".repeat(64)], "co_agents":{}, "trusted_users":[]},
        "watches":[{"id":7,"interval_secs":600,"filter":{"kinds":[1]}}]
    });
    let config_b64 = opencrab_nostr_gateway::config::canonicalize_config_b64(
        &base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&config).unwrap()),
    )
    .unwrap();
    let digest = format!(
        "{:x}",
        Sha256::digest(
            base64::engine::general_purpose::STANDARD
                .decode(&config_b64)
                .unwrap()
        )
    );
    core.execute(
        "UPDATE gate_instances SET kind_id='nostr',config_b64=?1,config_digest=?2",
        params![config_b64, digest],
    )
    .unwrap();
    core.execute("UPDATE gate_bindings SET address='nostr-agent-a'", [])
        .unwrap();
    core.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .unwrap();
    drop(core);
    let old = Connection::open(&gateway_path).unwrap();
    old.execute_batch("CREATE TABLE gateway_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        CREATE TABLE instances(agent_id TEXT PRIMARY KEY,agent_name TEXT NOT NULL,secret_key TEXT NOT NULL,relays_json TEXT NOT NULL,filter_json TEXT NOT NULL,enabled INTEGER NOT NULL,owner_pubkey TEXT NOT NULL DEFAULT '',self_pubkey TEXT NOT NULL DEFAULT '',updated_at TEXT NOT NULL);
        CREATE TABLE watches(watch_id INTEGER PRIMARY KEY,agent_id TEXT NOT NULL,session_id TEXT NOT NULL,interval_secs INTEGER NOT NULL,filter_json TEXT NOT NULL,created_at TEXT NOT NULL,UNIQUE(agent_id,session_id));
        CREATE TABLE allow_identities(agent_id TEXT NOT NULL,role TEXT NOT NULL,external_id TEXT NOT NULL,mapped_agent_id TEXT,PRIMARY KEY(agent_id,role,external_id));
        INSERT INTO gateway_meta VALUES ('legacy_import_v1','2026');
        INSERT INTO instances VALUES ('agent-a','A','test-signing-secret','[\"wss://example.invalid\"]','{\"kinds\":[1]}',1,'','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','2026');
        INSERT INTO watches VALUES (7,'agent-a','session-1',600,'{\"authors\":[],\"keywords\":[],\"kinds\":[1]}','2026');").unwrap();
    old.execute(
        "INSERT INTO allow_identities VALUES ('agent-a','owner',?1,NULL)",
        ["b".repeat(64)],
    )
    .unwrap();
    drop(old);
    write_secret(
        &key_path,
        &base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
    );
    let approval = Approval {
        version: 1,
        operation_id: "00000000-0000-4000-8000-000000000009".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        core_user_version: 56,
        source_core_sha256: source::file_sha256(&core_path).unwrap(),
        destinations: vec![Destination {
            kind_id: "nostr".into(),
            path_id: "nostr-primary".into(),
            schema: "s5-nostr-v1".into(),
        }],
        identity_dispositions: vec![],
        channel_edges: vec![],
        watch_edges: vec![],
        credential_sources: vec![CredentialSource {
            instance_id: "11111111-1111-4111-8111-111111111111".into(),
            source: "legacy-nostr-store:instance:11111111-1111-4111-8111-111111111111".into(),
        }],
    };
    write_secure(&approval_path, &canonical::bytes(&approval).unwrap());
    let report = command::run_import(ImportArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        report_path: &report_path,
        backup_dir: &backup_dir,
        inputs: Inputs {
            paths: BTreeMap::from([(
                ("nostr".into(), "nostr-primary".into()),
                gateway_path.clone(),
            )]),
            master_keys: BTreeMap::from([("nostr".into(), key_path)]),
            credential_files: BTreeMap::new(),
        },
    })
    .expect("existing gateway-owned data must migrate without manual reconstruction");
    assert_eq!(report.destinations[0]["counts"]["instances"], 1);
    let backup =
        opencrab_gateway_migrate::backup::database_path(&backup_dir, "nostr", "nostr-primary");
    let original = Connection::open(backup).unwrap();
    assert_eq!(
        original
            .query_row("SELECT COUNT(*) FROM watches", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(original);
    let reopened = opencrab_nostr_gateway::store::NostrStore::open(&gateway_path).unwrap();
    drop(reopened);
    let migrated = Connection::open(&gateway_path).unwrap();
    let (stable_id, envelope): (String, String) = migrated
        .query_row(
            "SELECT instance_id,credential_envelope FROM instances",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(stable_id, "11111111-1111-4111-8111-111111111111");
    assert_ne!(envelope, "test-signing-secret");
    assert_eq!(
        migrated
            .query_row(
                "SELECT COUNT(*) FROM identity_projections WHERE role='owner'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        migrated
            .query_row("SELECT COUNT(*) FROM watches", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1,
        "old watch data must not disappear"
    );
    assert!(!fs::read_to_string(report_path)
        .unwrap()
        .contains("test-signing-secret"));
}
