#[test]
fn s8_core_only_nostr_projects_historical_owner_before_trusted_admission() {
    core_only_nostr_owner_fixture(false, false);
}

#[test]
fn s8_core_only_nostr_converts_bound_historical_watch_session_id() {
    core_only_nostr_owner_fixture(true, false);
}

#[test]
fn s8_core_only_nostr_refuses_mismatched_historical_watch_before_backup() {
    core_only_nostr_owner_fixture(true, true);
}

fn core_only_nostr_owner_fixture(historical_watch_session: bool, mismatched_session: bool) {
    use opencrab_gate_client::wire::SaidCaller;
    use opencrab_nostr_gateway::{admission::admit, config::parse_instance_config, map::WatchEvent};

    let temp = tempfile::tempdir().unwrap();
    let core_path = temp.path().join("core.db");
    let gateway_path = temp.path().join("nostr.db");
    let key_path = temp.path().join("nostr.key");
    let approval_path = temp.path().join("approval.json");
    let report_path = temp.path().join("report.json");
    let verification_path = temp.path().join("verification.json");
    seed_core(&core_path);
    let core = Connection::open(&core_path).unwrap();
    core.execute_batch("DELETE FROM agent_discord_config; DELETE FROM channel_config; DELETE FROM trusted_users;").unwrap();
    core.execute("INSERT INTO agents(agent_id,name,persona_name,instructions,created_at,updated_at) VALUES ('agent-b','B','B','','2026','2026')", []).unwrap();
    core.execute("INSERT INTO sessions(id,theme,created_at,updated_at) VALUES ('session-2','t','2026','2026')", []).unwrap();
    core.execute("INSERT INTO agent_sessions(agent_id,session_id) VALUES ('agent-b','session-2')", []).unwrap();
    let owner = "b".repeat(64);
    let other = "c".repeat(64);
    let followee = "d".repeat(64);
    let configs = [
        ("agent-a", "11111111-1111-4111-8111-111111111111", "くらぶ", "a".repeat(64), true),
        ("agent-b", "22222222-2222-4222-8222-222222222222", "のすたろう", "e".repeat(64), false),
    ];
    for (agent, instance, name, self_key, enabled) in &configs {
        let config = serde_json::json!({"name":name,"relays":["wss://example.invalid"],
            "self_pubkey":self_key,"filter":{"kinds":[1]},
            "access":{"followees":[followee]},
            "watches":if *enabled { vec![if historical_watch_session { serde_json::json!({"id":7,"session_id":if mismatched_session { "session-2" } else { "session-1" },"interval_secs":600,"filter":{"kinds":[1]}}) } else { serde_json::json!({"id":7,"interval_secs":600,"filter":{"kinds":[1]}}) }] } else { vec![] }});
        let raw = base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&config).unwrap());
        let b64 = if *enabled && historical_watch_session { raw } else {
            opencrab_nostr_gateway::config::canonicalize_config_b64(&raw).unwrap()
        };
        let digest = format!("{:x}", Sha256::digest(base64::engine::general_purpose::STANDARD.decode(&b64).unwrap()));
        if *enabled {
            core.execute("UPDATE gate_instances SET kind_id='nostr',config_b64=?1,config_digest=?2", params![b64,digest]).unwrap();
            core.execute("UPDATE gate_bindings SET address='nostr-agent-a'", []).unwrap();
        } else {
            let subject: i64 = core.query_row("SELECT subject_id FROM agents WHERE agent_id=?1", [agent], |r| r.get(0)).unwrap();
            core.execute("INSERT INTO gate_instances(instance_id,kind_id,subject_id,revision,enabled,config_b64,config_digest,created_at,updated_at,association_grandfathered) VALUES (?1,'nostr',?2,1,0,?3,?4,1,1,1)", params![instance,subject,b64,digest]).unwrap();
            core.execute("INSERT INTO gate_bindings(binding_id,instance_id,address,created_at,session_id) VALUES ('binding-2',?1,'nostr-agent-b',1,'session-2')", [instance]).unwrap();
        }
    }
    core.execute_batch("CREATE TABLE agent_nostr_config(agent_id TEXT PRIMARY KEY,secret_key TEXT,relays_json TEXT,filter_json TEXT,enabled INTEGER,updated_at TEXT,owner_pubkey TEXT,self_pubkey TEXT);").unwrap();
    for (agent, _, _, self_key, enabled) in &configs {
        core.execute("INSERT INTO agent_nostr_config VALUES (?1,?2,'[\"wss://example.invalid\"]','{\"kinds\":[1]}',?3,'2026',?4,?5)", params![agent,if *enabled { "test-signing-secret" } else { "" },enabled,owner,self_key]).unwrap();
    }
    for (id, user) in [("tu-owner", &owner), ("tu-other", &other)] {
        core.execute("INSERT INTO trusted_users VALUES (?1,?2,'agent-a','user','owner','2026','Trusted','nostr')", params![id,user]).unwrap();
    }
    core.execute("INSERT INTO trusted_co_agents(id,agent_id,co_agent_id,created_by,created_at) VALUES ('co-1','agent-a','agent-b','owner','2026')", []).unwrap();
    core.execute("INSERT INTO session_watches(id,session_id,agent_id,interval_secs,filter_json,created_at) VALUES (7,'session-1','agent-a',600,'{\"authors\":[],\"keywords\":[],\"kinds\":[1]}','2026')", []).unwrap();
    core.execute("INSERT INTO tool_logs(agent_id,session_id,tool_name,args_json,outcome,result_text) VALUES ('agent-a','session-1','history-test','{}','done','retained')", []).unwrap();
    core.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;").unwrap();
    drop(core);
    let original_counts = protected_counts(&core_path);
    drop(opencrab_nostr_gateway::store::NostrStore::open(&gateway_path).unwrap());
    write_secret(&key_path, &base64::engine::general_purpose::STANDARD.encode([7u8;32]));
    let rows = source::validate(&source::open_read_only(&core_path).unwrap()).unwrap();
    let approval = Approval {
        version:1, operation_id:"00000000-0000-4000-8000-000000000019".into(),
        created_at:"2026-01-01T00:00:00Z".into(), core_user_version: 58,
        source_core_sha256:source::file_sha256(&core_path).unwrap(),
        destinations:vec![Destination {kind_id:"nostr".into(),path_id:"nostr-primary".into(),schema:"s5-nostr-v1".into()}],
        identity_dispositions:rows.iter().filter(|r| r.table=="trusted_users").map(|r| IdentityDisposition {
            source_fingerprint:r.fingerprint.clone(),edges:vec![IdentityEdge::Gateway {kind_id:"nostr".into(),instance_id:configs[0].1.into()}]
        }).collect(), channel_edges:vec![],
        watch_edges:rows.iter().filter(|r| r.table=="session_watches").map(|r| opencrab_gateway_migrate::manifest::WatchEdge {source_fingerprint:r.fingerprint.clone(),instance_id:configs[0].1.into()}).collect(),
        credential_sources:vec![CredentialSource {instance_id:configs[0].1.into(),source:"legacy-core:agent_nostr_config:agent-a".into()}],
    };
    write_secure(&approval_path, &canonical::bytes(&approval).unwrap());
    let backup_dir = temp.path().join("backups");
    let inputs = || Inputs {paths:BTreeMap::from([(("nostr".into(),"nostr-primary".into()),gateway_path.clone())]),master_keys:BTreeMap::from([("nostr".into(),key_path.clone())]),credential_files:BTreeMap::new()};
    let imported = command::run_import(ImportArgs {core_path:&core_path,approval_path:&approval_path,report_path:&report_path,backup_dir:&backup_dir,inputs:inputs()});
    if mismatched_session {
        assert!(imported.unwrap_err().to_string().contains("historical Nostr watch source mismatch"));
        assert!(!backup_dir.exists());
        return;
    }
    assert!(imported.is_ok(), "S8 must import the bound historical Nostr watch at its production entry: {:?}", imported.err().map(|error| error.to_string()));
    command::run_project(ProjectArgs {core_path:&core_path,approval_path:&approval_path,import_report_path:&report_path,verification_path:&verification_path,destination_paths:inputs().paths}).unwrap();
    let gateway = Connection::open(&gateway_path).unwrap();
    for (_, instance, name, self_key, _) in &configs {
        let b64: String = gateway.query_row("SELECT config_b64 FROM instances WHERE instance_id=?1",[instance],|r|r.get(0)).unwrap();
        let config = parse_instance_config(&base64::engine::general_purpose::STANDARD.decode(b64).unwrap()).unwrap();
        assert_eq!(config.name.as_deref(),Some(*name));
        let event = |pubkey: &str| WatchEvent { id:"f".repeat(64),pubkey:pubkey.into(),npub:None,note_id:None,created_at:1,kind:1,content:"くらぶ".into(),tags:vec![] };
        assert_eq!(admit(&event(&owner),self_key,&config.access),Some(SaidCaller::Owner));
        assert_eq!(admit(&event(&followee),self_key,&config.access),Some(SaidCaller::Agent));
        if *instance==configs[0].1 {
            assert_eq!(admit(&event(&other),self_key,&config.access),Some(SaidCaller::TrustedUser));
            assert_eq!(config.watches[0].id,7);
            assert_eq!(admit(&event(&configs[1].3),self_key,&config.access),Some(SaidCaller::CoAgent {agent_id:"agent-b".into()}));
        } else { assert_eq!(admit(&event(&other),self_key,&config.access),None); }
        let core_b64:String=Connection::open(&core_path).unwrap().query_row("SELECT config_b64 FROM gate_instances WHERE instance_id=?1",[instance],|r|r.get(0)).unwrap();
        let gateway_b64:String=gateway.query_row("SELECT config_b64 FROM instances WHERE instance_id=?1",[instance],|r|r.get(0)).unwrap();
        assert_eq!(core_b64,gateway_b64);
    }
    assert_eq!(protected_counts(&core_path),original_counts);
    assert_eq!(gateway.query_row("SELECT COUNT(*) FROM legacy_identity_sources",[],|r|r.get::<_,i64>(0)).unwrap(),2);
    assert_eq!(gateway.query_row("SELECT COUNT(*) FROM identity_projections WHERE role='owner'",[],|r|r.get::<_,i64>(0)).unwrap(),2);
    assert_eq!(gateway.query_row("SELECT relationship_id FROM identity_projections WHERE role='co_agent'",[],|r|r.get::<_,String>(0)).unwrap(),"agent-b");
    assert_eq!(Connection::open(&core_path).unwrap().query_row("SELECT result_text FROM tool_logs WHERE agent_id='agent-a'",[],|r|r.get::<_,String>(0)).unwrap(),"retained");
    command::run_import(ImportArgs {core_path:&core_path,approval_path:&approval_path,report_path:&report_path,backup_dir:&backup_dir,inputs:inputs()}).unwrap();
    command::run_project(ProjectArgs {core_path:&core_path,approval_path:&approval_path,import_report_path:&report_path,verification_path:&verification_path,destination_paths:inputs().paths}).unwrap();
}
