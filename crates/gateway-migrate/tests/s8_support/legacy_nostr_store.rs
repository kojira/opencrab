#[test]
fn s8_migrates_existing_nostr_gateway_store_without_losing_identity_or_watch() {
    legacy_nostr_gateway_fixture(false, false, false, None, false);
}

#[test]
fn s8_decrypts_existing_nostr_gateway_envelope_without_double_encrypting() {
    legacy_nostr_gateway_fixture(true, false, false, None, false);
}

#[test]
fn s8_refuses_unmapped_old_nostr_gateway_instance_without_backup_or_write() {
    legacy_nostr_gateway_fixture(false, true, false, None, false);
}

#[test]
fn s8_preserves_gateway_only_nostr_signing_key_rotation() {
    legacy_nostr_gateway_fixture(false, false, true, None, false);
}

#[test]
fn s8_preserves_old_gateway_settings_when_legacy_core_config_is_stale() {
    for field in ["relays_json", "filter_json", "enabled", "owner_pubkey", "self_pubkey"] {
        legacy_nostr_gateway_fixture(false, false, false, Some(field), false);
    }
}

#[test]
fn s8_refuses_conflicting_old_gateway_and_core_watch_sessions_before_backup() {
    legacy_nostr_gateway_fixture(false, false, false, None, true);
}

#[test]
fn s8_preserves_gateway_only_nostr_relay_change_in_core_and_destination() {
    legacy_nostr_gateway_fixture_with_updates(false, false, false, None, false, true, false);
}

#[test]
fn s8_keeps_removed_nostr_sender_inert_despite_stale_core_identity() {
    legacy_nostr_gateway_fixture_with_updates(false, false, false, None, false, false, true);
}

fn legacy_nostr_gateway_fixture(
    encrypted_source: bool,
    missing_association: bool,
    credential_conflict: bool,
    core_config_conflict: Option<&str>,
    watch_session_conflict: bool,
) {
    legacy_nostr_gateway_fixture_with_updates(encrypted_source, missing_association, credential_conflict,
        core_config_conflict, watch_session_conflict, false, false);
}

fn legacy_nostr_gateway_fixture_with_updates(
    encrypted_source: bool,
    missing_association: bool,
    credential_conflict: bool,
    core_config_conflict: Option<&str>,
    watch_session_conflict: bool,
    gateway_relay_change: bool,
    stale_core_sender: bool,
) {
    let temp = tempfile::tempdir().unwrap();
    let core_path = temp.path().join("core.db");
    let gateway_path = temp.path().join("nostr.db");
    let key_path = temp.path().join("nostr.key");
    let approval_path = temp.path().join("approval.json");
    let report_path = temp.path().join("report.json");
    let verification_path = temp.path().join("verification.json");
    let backup_dir = temp.path().join("backup");
    seed_core(&core_path);
    let core = Connection::open(&core_path).unwrap();
    core.execute_batch(
        "DELETE FROM agent_discord_config; DELETE FROM channel_config; DELETE FROM trusted_users;",
    )
    .unwrap();
    if stale_core_sender {
        core.execute("INSERT INTO trusted_users VALUES ('tu-stale',?1,'agent-a','user','owner','2026','Stale','nostr')",
            ["c".repeat(64)]).unwrap();
    }
    let config = serde_json::json!({
        "relays":["wss://example.invalid"], "filter":{"kinds":[1]},
        "self_pubkey":"a".repeat(64), "name":"A",
        "access":{"owner":["b".repeat(64)], "co_agents":{}, "trusted_users":if stale_core_sender { vec!["c".repeat(64)] } else { Vec::new() }},
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
    if credential_conflict || core_config_conflict.is_some() {
        core.execute_batch("CREATE TABLE agent_nostr_config(agent_id TEXT PRIMARY KEY,secret_key TEXT,relays_json TEXT,filter_json TEXT,enabled INTEGER,updated_at TEXT,owner_pubkey TEXT,self_pubkey TEXT);").unwrap();
        core.execute(
            "INSERT INTO agent_nostr_config VALUES ('agent-a',?1,?2,?3,1,'2026','',?4)",
            params![if credential_conflict { "different-signing-secret" } else { "test-signing-secret" },
                "[\"wss://example.invalid\"]", "{\"kinds\":[1]}", "a".repeat(64)],
        ).unwrap();
        if let Some(field) = core_config_conflict {
            let value = match field {
                "relays_json" => "[\"wss://other.invalid\"]".to_string(),
                "filter_json" => "{\"kinds\":[2]}".to_string(),
                "enabled" => "0".to_string(),
                "owner_pubkey" => "c".repeat(64),
                "self_pubkey" => "d".repeat(64),
                _ => unreachable!(),
            };
            core.execute(&format!("UPDATE agent_nostr_config SET {field}=?1"), [value]).unwrap();
        }
    }
    let watch_fingerprint = if watch_session_conflict {
        core.execute_batch("INSERT INTO sessions(id,theme,created_at,updated_at) VALUES ('session-2','t','2026','2026');
            INSERT INTO agent_sessions(agent_id,session_id) VALUES ('agent-a','session-2');
            INSERT INTO gate_bindings(binding_id,instance_id,address,created_at,session_id) VALUES ('binding-2','11111111-1111-4111-8111-111111111111','nostr-agent-a-second',1,'session-2');
            INSERT INTO session_watches(id,session_id,agent_id,interval_secs,filter_json,created_at) VALUES (7,'session-2','agent-a',600,'{\"authors\":[],\"keywords\":[],\"kinds\":[1]}','2026');").unwrap();
        Some(source::validate(&core).unwrap().into_iter().find(|row| row.table == "session_watches").unwrap().fingerprint)
    } else { None };
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
    if gateway_relay_change {
        old.execute("UPDATE instances SET relays_json='[\"wss://changed.invalid\"]' WHERE agent_id='agent-a'", []).unwrap();
    }
    if encrypted_source {
        let source_envelope =
            opencrab_nostr_gateway::secret_store::encrypt(b"test-signing-secret", &[7u8; 32])
                .unwrap();
        old.execute(
            "UPDATE instances SET secret_key=?1 WHERE agent_id='agent-a'",
            [source_envelope],
        )
        .unwrap();
    }
    if missing_association {
        old.execute(
            "UPDATE instances SET agent_id='unmapped' WHERE agent_id='agent-a'",
            [],
        )
        .unwrap();
    }
    drop(old);
    let original_gateway_hash = source::file_sha256(&gateway_path).unwrap();
    write_secret(
        &key_path,
        &base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
    );
    let stale_dispositions = if stale_core_sender {
        vec![IdentityDisposition {
            source_fingerprint: source::validate(&Connection::open(&core_path).unwrap()).unwrap()
                .into_iter().find(|row| row.table == "trusted_users").unwrap().fingerprint,
            edges: vec![IdentityEdge::Gateway {
                kind_id: "nostr".into(),
                instance_id: "11111111-1111-4111-8111-111111111111".into(),
            }],
        }]
    } else { Vec::new() };
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
        identity_dispositions: stale_dispositions,
        channel_edges: vec![],
        watch_edges: watch_fingerprint.map(|source_fingerprint| opencrab_gateway_migrate::manifest::WatchEdge {
            source_fingerprint,
            instance_id: "11111111-1111-4111-8111-111111111111".into(),
        }).into_iter().collect(),
        credential_sources: vec![CredentialSource {
            instance_id: "11111111-1111-4111-8111-111111111111".into(),
            source: "legacy-nostr-store:instance:11111111-1111-4111-8111-111111111111".into(),
        }],
    };
    write_secure(&approval_path, &canonical::bytes(&approval).unwrap());
    let result = command::run_import(ImportArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        report_path: &report_path,
        backup_dir: &backup_dir,
        inputs: Inputs {
            paths: BTreeMap::from([(
                ("nostr".into(), "nostr-primary".into()),
                gateway_path.clone(),
            )]),
            master_keys: BTreeMap::from([("nostr".into(), key_path.clone())]),
            credential_files: BTreeMap::new(),
        },
    });
    if watch_session_conflict {
        assert!(result.unwrap_err().to_string().contains("old Nostr watch session conflict"), "must refuse contradictory watch session");
        assert!(!backup_dir.exists(), "source conflicts must refuse before backup");
        assert_eq!(source::file_sha256(&gateway_path).unwrap(), original_gateway_hash);
        return;
    }
    if missing_association {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("old Nostr instance missing"));
        assert!(
            !backup_dir.exists(),
            "invalid association must refuse before backup"
        );
        assert_eq!(
            source::file_sha256(&gateway_path).unwrap(),
            original_gateway_hash
        );
        return;
    }
    let report =
        result.expect("existing gateway-owned data must migrate without manual reconstruction");
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
    let retained_envelope: String = migrated
        .query_row(
            "SELECT secret_key FROM legacy_nostr_instances WHERE agent_id='agent-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        retained_envelope, envelope,
        "upgraded store must not retain plaintext"
    );
    assert_eq!(
        opencrab_nostr_gateway::secret_store::decrypt(&envelope, &[7u8; 32])
            .unwrap()
            .as_slice(),
        b"test-signing-secret"
    );
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
    assert!(!fs::read_to_string(&report_path)
        .unwrap()
        .contains("test-signing-secret"));
    let replay = command::run_import(ImportArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        report_path: &report_path,
        backup_dir: &backup_dir,
        inputs: Inputs {
            paths: BTreeMap::from([(("nostr".into(), "nostr-primary".into()), gateway_path)]),
            master_keys: BTreeMap::from([("nostr".into(), key_path)]),
            credential_files: BTreeMap::new(),
        },
    })
    .expect("successful old Nostr store conversion must rerun without re-encrypting");
    assert_eq!(
        replay.destination_manifest_sha256,
        report.destination_manifest_sha256
    );
    let verified = command::run_project(ProjectArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        import_report_path: &report_path,
        verification_path: &verification_path,
        destination_paths: BTreeMap::from([(
            ("nostr".into(), "nostr-primary".into()),
            temp.path().join("nostr.db"),
        )]),
    })
    .expect("existing user must complete S8 projection after old Nostr store conversion");
    assert_eq!(verified["core_projection"]["deliveries"]["row_count"], 1);
    if credential_conflict {
        let core = Connection::open(&core_path).unwrap();
        let retained: String = core.query_row("SELECT secret_key FROM agent_nostr_config WHERE agent_id='agent-a'", [], |r| r.get(0)).unwrap();
        assert_eq!(retained, "different-signing-secret", "stale source history must remain untouched");
        assert_eq!(opencrab_nostr_gateway::secret_store::decrypt(&envelope, &[7u8; 32]).unwrap().as_slice(), b"test-signing-secret");
    }
    if gateway_relay_change || stale_core_sender {
        let core = Connection::open(&core_path).unwrap();
        let (revision, config_b64): (i64, String) = core.query_row(
            "SELECT revision,config_b64 FROM gate_instances WHERE instance_id='11111111-1111-4111-8111-111111111111'",
            [], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        let config: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD.decode(&config_b64).unwrap()).unwrap();
        if gateway_relay_change {
            assert_eq!(revision, 2, "stopped projection must revise only changed Nostr config");
            assert_eq!(config["relays"], serde_json::json!(["wss://changed.invalid"]));
            let destination_config: String = migrated.query_row("SELECT config_b64 FROM instances", [], |r| r.get(0)).unwrap();
            assert_eq!(config_b64, destination_config);
        } else {
            assert_eq!(revision, 2, "removing a stale core sender must revise admission config once");
        }
        if stale_core_sender {
            assert_eq!(config["access"]["trusted_users"], serde_json::json!([]));
            assert_eq!(migrated.query_row("SELECT COUNT(*) FROM identity_projections WHERE external_id=?1", ["c".repeat(64)], |r| r.get::<_, i64>(0)).unwrap(), 0);
            assert_eq!(migrated.query_row("SELECT COUNT(*) FROM legacy_identity_sources WHERE id='tu-stale'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        }
        assert_eq!(core.query_row("SELECT COUNT(*) FROM gate_bindings WHERE binding_id='binding-1'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    }
}
