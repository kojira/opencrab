use std::path::Path;

#[test]
fn s8_import_and_project_are_offline_idempotent_and_preserve_core_rows() {
    projected_discord_fixture(false, false, false, false);
}

#[test]
fn s10_verify_freeze_is_read_only_for_projected_discord_fixture() {
    projected_discord_fixture(true, false, false, false);
}

#[test]
fn s10_verify_freeze_accepts_post_qc_heartbeat_last_fired_advance() {
    projected_discord_fixture(true, true, false, false);
}

#[test]
fn s10_cleanup_after_freeze_preserves_retained_core_state() {
    projected_discord_fixture(true, true, true, false);
}

#[test]
fn s10_cleanup_refuses_gateway_identity_change_after_freeze() {
    projected_discord_fixture(true, true, true, true);
}

fn advance_projected_heartbeat(path: &Path) {
    let conn = Connection::open(path).unwrap();
    opencrab_db::queries::set_session_last_fired(
        &conn,
        "agent-a",
        "session-1",
        "2026-01-01T00:10:00Z",
    )
    .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT last_fired_at FROM session_heartbeat_config WHERE agent_id='agent-a' AND session_id='session-1'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
        .as_deref(),
        Some("2026-01-01T00:10:00Z"),
        "the normal fire path must advance the projected target before post-QC freeze"
    );
}

fn assert_frozen_cleanup(root: &Path, mutate_gateway: bool) {
    let core_path = &root.join("core.db");
    let discord_path = &root.join("discord.db");
    let approval_path = &root.join("approval.json");
    let report_path = &root.join("report.json");
    let verification_path = &root.join("verification.json");
    let freeze_dir = &root.join("post-qc-freeze");
    let freeze_manifest_path = &root.join("freeze.json");
    let core = Connection::open(core_path).unwrap();
    let gateway = Connection::open(discord_path).unwrap();
    let core_identity = core.query_row(
        "SELECT id,user_id,agent_id,permission,created_by,created_at,display_name,platform FROM trusted_users WHERE id='tu-discord'",
        [],
        |row| (0..8).map(|index| row.get::<_, String>(index)).collect::<rusqlite::Result<Vec<_>>>(),
    ).unwrap();
    let gateway_identity = gateway.query_row(
        "SELECT id,user_id,agent_id,permission,created_by,created_at,display_name,platform FROM legacy_identity_sources WHERE instance_id='11111111-1111-4111-8111-111111111111' AND id='tu-discord'",
        [],
        |row| (0..8).map(|index| row.get::<_, String>(index)).collect::<rusqlite::Result<Vec<_>>>(),
    ).unwrap();
    assert_eq!(gateway_identity, core_identity, "all eight source identity fields must be present before deletion");
    let before = retained_cleanup_rows(&core);
    let marker = core.query_row(
        "SELECT operation_id,approval_sha256,backup_set_sha256,source_core_sha256,source_fingerprint_sha256,subject_lineage_sha256,heartbeat_lineage_sha256,initial_projection_sha256,destination_manifest_sha256 FROM separation_migrations",
        [],
        |row| (0..9).map(|index| row.get::<_, String>(index)).collect::<rusqlite::Result<Vec<_>>>(),
    ).unwrap();
    drop(gateway);
    drop(core);
    let core_before = source::file_sha256(core_path).unwrap();
    if mutate_gateway {
        let gateway = Connection::open(discord_path).unwrap();
        gateway.execute("UPDATE legacy_identity_sources SET display_name='changed-after-freeze' WHERE id='tu-discord'", []).unwrap();
        drop(gateway);
    }

    let cleanup = std::process::Command::new(env!("CARGO_BIN_EXE_opencrab-gateway-migrate"))
        .arg("clean-legacy-state")
        .args(["--core-db", core_path.to_str().unwrap()])
        .args(["--approval", approval_path.to_str().unwrap()])
        .args(["--import-report", report_path.to_str().unwrap()])
        .args(["--verification", verification_path.to_str().unwrap()])
        .args(["--freeze-dir", freeze_dir.to_str().unwrap()])
        .args(["--freeze-manifest", freeze_manifest_path.to_str().unwrap()])
        .args(["--destination", &format!("discord=discord-primary={}", discord_path.display())])
        .output()
        .unwrap();
    if mutate_gateway {
        assert!(!cleanup.status.success(), "cleanup must reject changed gateway data");
        assert_eq!(source::file_sha256(core_path).unwrap(), core_before,
            "rejected cleanup must not modify the retained core");
        let core = Connection::open(core_path).unwrap();
        let still_present: i64 = core.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='trusted_users'", [], |row| row.get(0)).unwrap();
        assert_eq!(still_present, 1, "legacy identity must remain after rejected cleanup");
        return;
    }
    assert!(
        cleanup.status.success(),
        "S10 guarded cleanup must accept the unchanged full-field source and valid post-QC freeze: {}",
        String::from_utf8_lossy(&cleanup.stderr)
    );
    let core = Connection::open(core_path).unwrap();
    assert_eq!(retained_cleanup_rows(&core), before, "cleanup changed retained subjects, sessions, bindings, history, delivery or heartbeat");
    let unchanged_marker = core.query_row(
        "SELECT operation_id,approval_sha256,backup_set_sha256,source_core_sha256,source_fingerprint_sha256,subject_lineage_sha256,heartbeat_lineage_sha256,initial_projection_sha256,destination_manifest_sha256 FROM separation_migrations",
        [],
        |row| (0..9).map(|index| row.get::<_, String>(index)).collect::<rusqlite::Result<Vec<_>>>(),
    ).unwrap();
    assert_eq!(unchanged_marker, marker, "cleanup must not alter the immutable projection marker");
    let removed: i64 = core.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('trusted_users','channel_config','session_watches','agent_discord_config','agent_nostr_config')",
        [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(removed, 0, "only after full-field disposition may legacy concrete source tables be removed");
    let cleanup_record: (String, String, String) = core.query_row(
        "SELECT freeze_id,projection_operation_id,freeze_manifest_sha256 FROM separation_cleanup_applied",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(freeze_manifest_path).unwrap()).unwrap();
    assert_eq!(cleanup_record, (
        manifest["freeze_id"].as_str().unwrap().to_owned(),
        "00000000-0000-4000-8000-000000000008".to_owned(),
        canonical::hash(&manifest).unwrap(),
    ), "the separate cleanup record must identify this exact frozen set and projection");
    assert_eq!(
        core.query_row("SELECT COUNT(*) FROM api_principals WHERE id='tu-rest'", [], |row| row.get::<_, i64>(0)).unwrap(),
        1,
        "the retained REST principal must not be deleted with mixed trusted_users"
    );
    drop(core);
    let restarted = opencrab_db::init_connection(core_path.to_str().unwrap())
        .expect("core initialization must accept an offline-cleaned database");
    let revived_legacy: i64 = restarted.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('trusted_users','channel_config','session_watches','agent_discord_config','agent_nostr_config')",
        [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(revived_legacy, 0, "core restart must not recreate cleaned legacy gateway tables");
    let rest = opencrab_db::queries::get_api_principal(&restarted, "rest-user", "agent-a")
        .expect("REST principal lookup must survive cleanup and restart");
    assert_eq!(rest.id, "tu-rest");
}

fn retained_cleanup_rows(core: &Connection) -> Vec<String> {
    [
        "SELECT CAST(subject_id AS TEXT)||':'||agent_id FROM agents WHERE agent_id='agent-a'",
        "SELECT id||':'||theme FROM sessions WHERE id='session-1'",
        "SELECT agent_id||':'||session_id FROM agent_sessions WHERE agent_id='agent-a' AND session_id='session-1'",
        "SELECT instance_id||':'||CAST(subject_id AS TEXT)||':'||config_digest FROM gate_instances WHERE instance_id='11111111-1111-4111-8111-111111111111'",
        "SELECT binding_id||':'||instance_id||':'||address||':'||session_id FROM gate_bindings WHERE binding_id='binding-1'",
        "SELECT agent_id||':'||session_id||':'||tool_name||':'||args_json||':'||outcome||':'||result_text FROM tool_logs WHERE tool_name='history-test'",
        "SELECT delivery_id||':'||binding_id||':'||payload_json||':'||state FROM deliveries WHERE delivery_id='delivery-1'",
        "SELECT agent_id||':'||session_id||':'||last_fired_at FROM session_heartbeat_config WHERE agent_id='agent-a' AND session_id='session-1'",
    ]
    .iter()
    .map(|sql| core.query_row(sql, [], |row| row.get::<_, String>(0)).unwrap())
    .collect()
}
