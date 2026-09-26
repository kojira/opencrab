use base64::Engine as _;
use opencrab_gateway_migrate::{
    canonical,
    command::{self, ImportArgs, ProjectArgs},
    destination::Inputs,
    manifest::{
        Approval, ChannelEdge, CredentialSource, Destination, IdentityDisposition, IdentityEdge,
    },
    source::{self, Cell},
};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};

#[test]
fn published_schema_56_source_fingerprints_are_stable() {
    let cases = [
        (
            "trusted_users",
            vec![
                t("id", "tu-1"),
                t("user_id", "42"),
                t("agent_id", "agent-a"),
                t("permission", "co-agent"),
                t("created_by", "owner"),
                t("created_at", "2026-01-01T00:00:00Z"),
                t("display_name", "Crab"),
                t("platform", "rest"),
            ],
            "2c6850f418281b0b4ede33a1a3ab379cef850e7a85dfd7d2679ee9209a5526a7",
        ),
        (
            "channel_config",
            vec![
                t("channel_id", "chan-1"),
                t("agent_id", "agent-a"),
                t("guild_id", "guild-1"),
                t("channel_name", "General"),
                i("readable", 1),
                i("writable", 0),
                i("whitelisted", 1),
                i("heartbeat_enabled", 1),
                ("heartbeat_interval_secs".into(), Cell::Null),
                t("heartbeat_instructions", "Ping"),
                t("updated_at", "2026-01-01T00:00:00Z"),
            ],
            "e9099467f8ece3abb668571cab576378cf02d894f2eaf911005c85c5c50f6348",
        ),
        (
            "session_watches",
            vec![
                i("id", 7),
                t("session_id", "session-a"),
                t("agent_id", "agent-a"),
                i("interval_secs", 600),
                t("filter_json", "{\"authors\":[\"abc\"]}"),
                t("created_at", "2026-01-01T00:00:00Z"),
            ],
            "8150c628708b328fe4c4dfdf816caa806738fda29ea2fac1304532c62ae8911d",
        ),
        (
            "agent_discord_config",
            vec![
                t("agent_id", "agent-a"),
                t("bot_token", "test-token"),
                t("owner_discord_id", "42"),
                i("enabled", 1),
                t("updated_at", "2026-01-01T00:00:00Z"),
                t("bot_user_id", "99"),
            ],
            "0db7c5133b00d98943329f5fa1cb9fff1bc6f262f37a111875246f831da5514e",
        ),
        (
            "agent_nostr_config",
            vec![
                t("agent_id", "agent-a"),
                t("secret_key", "test-secret"),
                t("relays_json", "[\"wss://relay.example\"]"),
                t("filter_json", "{\"kinds\":[1]}"),
                i("enabled", 1),
                t("updated_at", "2026-01-01T00:00:00Z"),
                t("owner_pubkey", "owner-pub"),
                t("self_pubkey", "self-pub"),
            ],
            "f37159af0363c135d48435ed493b34f05ecea5ae3869294712ec82a6214cc788",
        ),
    ];
    for (table, columns, expected) in cases {
        assert_eq!(source::fingerprint(table, &columns), expected, "{table}");
    }
}

#[test]
fn source_profile_refuses_neighbor_versions_and_missing_required_column() {
    for version in [55, 57] {
        let conn = opencrab_db::init_memory().unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
        assert!(source::validate(&conn)
            .unwrap_err()
            .to_string()
            .contains("exactly 56"));
    }
    let conn = opencrab_db::init_memory().unwrap();
    create_legacy_core_source_tables(&conn);
    conn.execute_batch("ALTER TABLE trusted_users ADD COLUMN surprise TEXT;")
        .unwrap();
    source::validate(&conn).expect("extra unrelated columns must not block migration");
    conn.execute_batch("ALTER TABLE trusted_users RENAME COLUMN user_id TO missing_user_id;")
        .unwrap();
    assert!(source::validate(&conn)
        .unwrap_err()
        .to_string()
        .contains("trusted_users"));
}
include!("s8_support/legacy_source_schema.rs");
include!("s8_support/legacy_nostr_store.rs");
include!("s8_support/projected.rs");
fn projected_discord_fixture(freeze: bool, fired: bool, cleanup: bool, changed: bool) {
    let temp = tempfile::tempdir().unwrap();
    let core_path = temp.path().join("core.db");
    let discord_path = temp.path().join("discord.db");
    let approval_path = temp.path().join("approval.json");
    let report_path = temp.path().join("report.json");
    let verification_path = temp.path().join("verification.json");
    let backup_dir = temp.path().join("backups");
    let key_path = temp.path().join("discord.key");
    seed_core(&core_path);
    if cleanup {
        let core = Connection::open(&core_path).unwrap();
        core.execute("INSERT INTO tool_logs(agent_id,session_id,tool_name,args_json,outcome,result_text) VALUES ('agent-a','session-1','history-test','{}','done','retained')", []).unwrap();
    }
    drop(opencrab_discord_gateway::store::DiscordStore::open(&discord_path).unwrap());
    write_secret(
        &key_path,
        &base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
    );
    let conn = source::open_read_only(&core_path).unwrap();
    let rows = source::validate(&conn).unwrap();
    drop(conn);
    let trusted = rows
        .iter()
        .find(|row| row.table == "trusted_users" && row.text("platform").unwrap() == "discord")
        .unwrap();
    let rest = rows
        .iter()
        .find(|row| row.table == "trusted_users" && row.text("platform").unwrap() == "rest")
        .unwrap();
    let channel = rows
        .iter()
        .find(|row| row.table == "channel_config")
        .unwrap();
    let mut dispositions = vec![
        IdentityDisposition {
            source_fingerprint: trusted.fingerprint.clone(),
            edges: vec![IdentityEdge::Gateway {
                kind_id: "discord".into(),
                instance_id: "11111111-1111-4111-8111-111111111111".into(),
            }],
        },
        IdentityDisposition {
            source_fingerprint: rest.fingerprint.clone(),
            edges: vec![IdentityEdge::ApiPrincipal],
        },
    ];
    dispositions.sort_by(|a, b| a.source_fingerprint.cmp(&b.source_fingerprint));
    let approval = Approval {
        version: 1,
        operation_id: "00000000-0000-4000-8000-000000000008".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        core_user_version: 56,
        source_core_sha256: source::file_sha256(&core_path).unwrap(),
        destinations: vec![Destination {
            kind_id: "discord".into(),
            path_id: "discord-primary".into(),
            schema: "s5-discord-v1".into(),
        }],
        identity_dispositions: dispositions,
        channel_edges: vec![ChannelEdge {
            source_fingerprint: channel.fingerprint.clone(),
            instance_id: "11111111-1111-4111-8111-111111111111".into(),
            binding_id: "binding-1".into(),
            session_id: "session-1".into(),
        }],
        watch_edges: vec![],
        credential_sources: vec![CredentialSource {
            instance_id: "11111111-1111-4111-8111-111111111111".into(),
            source: "legacy-core:agent_discord_config:agent-a".into(),
        }],
    };
    write_secure(&approval_path, &canonical::bytes(&approval).unwrap());
    let mut paths = BTreeMap::new();
    paths.insert(
        ("discord".into(), "discord-primary".into()),
        discord_path.clone(),
    );
    let mut keys = BTreeMap::new();
    keys.insert("discord".into(), key_path.clone());
    let before = protected_counts(&core_path);
    let report = command::run_import(ImportArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        backup_dir: &backup_dir,
        report_path: &report_path,
        inputs: Inputs {
            paths,
            master_keys: keys,
            credential_files: BTreeMap::new(),
        },
    })
    .unwrap();
    assert_eq!(
        protected_counts(&core_path),
        before,
        "import must keep core read-only"
    );
    assert_eq!(report.destinations[0]["counts"]["instances"], 1);
    let destination = Connection::open(&discord_path).unwrap();
    let has_legacy_identity_sources: bool = destination
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='legacy_identity_sources')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        has_legacy_identity_sources,
        "S8 must retain the gateway identity's original ID and metadata before S10 may delete its source"
    );
    let original_row: (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = destination
        .query_row(
            "SELECT id,user_id,agent_id,permission,created_by,created_at,display_name,platform \
             FROM legacy_identity_sources WHERE instance_id=?1 AND id='tu-discord'",
            ["11111111-1111-4111-8111-111111111111"],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        original_row,
        (
            "tu-discord".into(),
            "42".into(),
            "agent-a".into(),
            "owner".into(),
            "owner".into(),
            "2026".into(),
            "Crab".into(),
            "discord".into(),
        ),
        "the destination must retain all original source identity fields verbatim"
    );
    drop(destination);
    let rerun_report = command::run_import(ImportArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        backup_dir: &backup_dir,
        report_path: &report_path,
        inputs: Inputs {
            paths: BTreeMap::from([(
                ("discord".into(), "discord-primary".into()),
                discord_path.clone(),
            )]),
            master_keys: BTreeMap::from([("discord".into(), key_path.clone())]),
            credential_files: BTreeMap::new(),
        },
    })
    .unwrap();
    assert_eq!(
        rerun_report.destination_manifest_sha256, report.destination_manifest_sha256,
        "exact lost-response import rerun must accept its immutable report"
    );
    let project_args = || ProjectArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        import_report_path: &report_path,
        verification_path: &verification_path,
        destination_paths: BTreeMap::from([(
            ("discord".into(), "discord-primary".into()),
            discord_path.clone(),
        )]),
    };
    let verification = command::run_project(project_args()).unwrap();
    assert_eq!(
        verification["core_projection"]["deliveries"]["row_count"],
        1
    );
    let conn = Connection::open(&core_path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM api_principals WHERE user_id='rest-user'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM trusted_users", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM deliveries", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM separation_migrations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(conn);
    let original_verification = fs::read(&verification_path).unwrap();
    let projected_hash = source::file_sha256(&core_path).unwrap();
    let replay = command::run_project(project_args())
        .expect("completed project rerun must accept its existing verification");
    assert_eq!(
        fs::read(&verification_path).unwrap(),
        original_verification,
        "completed rerun must preserve the original verification bytes"
    );
    assert_eq!(
        replay, verification,
        "completed rerun returns original evidence"
    );
    assert_eq!(
        source::file_sha256(&core_path).unwrap(),
        projected_hash,
        "already_applied rerun must keep core byte-identical"
    );
    assert_eq!(
        replay["core_projection"]["deliveries"], verification["core_projection"]["deliveries"],
        "rerun preserves delivery evidence"
    );
    let credential_source = report.destinations[0]["credentials"][0]["source"]
        .as_str()
        .expect("credential source descriptor");
    assert!(
        credential_source.starts_with("legacy-core:agent_discord_config"),
        "credential provenance category remains available for audit"
    );
    let leaking_artifacts = [
        ("import report", fs::read_to_string(&report_path).unwrap()),
        (
            "verification",
            fs::read_to_string(&verification_path).unwrap(),
        ),
    ]
    .into_iter()
    .filter(|(_, contents)| {
        ["agent-a", "rest-user", "test-token"]
            .iter()
            .any(|forbidden| contents.contains(forbidden))
    })
    .map(|(artifact, _)| artifact)
    .collect::<Vec<_>>();
    assert!(
        leaking_artifacts.is_empty(),
        "tool-produced artifacts must not expose raw agent/external identifiers or credential plaintext: {leaking_artifacts:?}"
    );

    if !freeze {
        return;
    }
    if fired {
        advance_projected_heartbeat(&core_path);
    }
    // S10 RED: the same fully projected disposable fixture is frozen as one
    // matched core-plus-participating-gateway set before cleanup can be offered.
    let freeze_dir = temp.path().join("post-qc-freeze");
    let records = opencrab_gateway_migrate::backup::create_or_load_set(
        &core_path,
        &[(approval.destinations[0].clone(), discord_path.clone())],
        &freeze_dir,
    )
    .unwrap();
    let marker_conn = source::open_read_only(&core_path).unwrap();
    let marker = marker_conn
        .query_row(
            "SELECT operation_id,approval_sha256,backup_set_sha256,source_core_sha256,\
             source_fingerprint_sha256,subject_lineage_sha256,heartbeat_lineage_sha256,\
             initial_projection_sha256,destination_manifest_sha256 \
             FROM separation_migrations WHERE operation_id=?1",
            [&approval.operation_id],
            |row| {
                (0..9)
                    .map(|index| row.get::<_, String>(index))
                    .collect::<rusqlite::Result<Vec<_>>>()
            },
        )
        .unwrap();
    drop(marker_conn);
    let freeze_manifest_path = temp.path().join("freeze.json");
    let freeze_id = "00000000-0000-4000-8000-000000000010";
    let snapshots = records
        .iter()
        .map(|record| {
            let mut entry = serde_json::to_value(record).unwrap();
            entry["freeze_id"] = serde_json::json!(freeze_id);
            let live_path = if record.kind_id == "core" {
                &core_path
            } else {
                &discord_path
            };
            entry["live_file_sha256"] = serde_json::json!(source::file_sha256(live_path).unwrap());
            assert_eq!(entry["freeze_id"], freeze_id);
            assert_eq!(
                record.logical_sha256,
                opencrab_gateway_migrate::backup::logical_sha256(live_path).unwrap()
            );
            entry
        })
        .collect::<Vec<_>>();
    let freeze_manifest = serde_json::json!({
        "version": 1,
        "freeze_id": freeze_id,
        "approval_sha256": report.approval_sha256,
        "projection_manifest_sha256": verification["manifest_sha256"],
        "projection_marker_sha256": canonical::hash(&marker).unwrap(),
        "identity_dispositions_sha256": canonical::hash(&approval.identity_dispositions).unwrap(),
        "snapshots": snapshots,
    });
    write_secure(
        &freeze_manifest_path,
        &canonical::value_bytes(&freeze_manifest).unwrap(),
    );
    let core_before_freeze = (
        source::file_sha256(&core_path).unwrap(),
        opencrab_gateway_migrate::backup::logical_sha256(&core_path).unwrap(),
    );
    let gateway_before_freeze = (
        source::file_sha256(&discord_path).unwrap(),
        opencrab_gateway_migrate::backup::logical_sha256(&discord_path).unwrap(),
    );
    let preflight = std::process::Command::new(env!("CARGO_BIN_EXE_opencrab-gateway-migrate"))
        .arg("verify-freeze")
        .args(["--core-db", core_path.to_str().unwrap()])
        .args(["--approval", approval_path.to_str().unwrap()])
        .args(["--import-report", report_path.to_str().unwrap()])
        .args(["--verification", verification_path.to_str().unwrap()])
        .args(["--freeze-dir", freeze_dir.to_str().unwrap()])
        .args(["--freeze-manifest", freeze_manifest_path.to_str().unwrap()])
        .args([
            "--destination",
            &format!("discord=discord-primary={}", discord_path.display()),
        ])
        .output()
        .unwrap();
    assert!(
        preflight.status.success(),
        "S10 verify-freeze must accept a matched projected fixture without writing: {}",
        String::from_utf8_lossy(&preflight.stderr)
    );
    assert_eq!(
        (
            source::file_sha256(&core_path).unwrap(),
            opencrab_gateway_migrate::backup::logical_sha256(&core_path).unwrap(),
        ),
        core_before_freeze,
        "read-only freeze preflight changed core"
    );
    assert_eq!(
        (
            source::file_sha256(&discord_path).unwrap(),
            opencrab_gateway_migrate::backup::logical_sha256(&discord_path).unwrap(),
        ),
        gateway_before_freeze,
        "read-only freeze preflight changed the participating gateway"
    );
    if cleanup {
        assert_frozen_cleanup(temp.path(), changed);
    }
}

#[test]
fn s8_refuses_retained_discord_config_with_only_nostr_instance_before_backup() {
    let temp = tempfile::tempdir().unwrap();
    let core_path = temp.path().join("core.db");
    let nostr_path = temp.path().join("nostr.db");
    let key_path = temp.path().join("nostr.key");
    let approval_path = temp.path().join("approval.json");
    let backup_dir = temp.path().join("backups");
    seed_core(&core_path);
    let conn = Connection::open(&core_path).unwrap();
    conn.execute_batch("DELETE FROM deliveries; DELETE FROM gate_bindings; DELETE FROM channel_config; DELETE FROM trusted_users;").unwrap();
    let config_b64 = opencrab_nostr_gateway::config::canonicalize_config_b64(
        &base64::engine::general_purpose::STANDARD.encode(
            serde_json::to_vec(&serde_json::json!({
                "relays": ["wss://example.invalid"],
                "self_pubkey": "aa".repeat(32),
                "name": "test-agent",
                "access": {"owner": [], "co_agents": {}, "trusted_users": []}
            }))
            .unwrap(),
        ),
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
    conn.execute(
        "UPDATE gate_instances SET kind_id='nostr',config_b64=?1,config_digest=?2",
        params![config_b64, digest],
    )
    .unwrap();
    conn.execute_batch("CREATE TABLE agent_nostr_config(agent_id TEXT,secret_key TEXT,relays_json TEXT,filter_json TEXT,enabled INTEGER,updated_at TEXT,owner_pubkey TEXT,self_pubkey TEXT);").unwrap();
    conn.execute("INSERT INTO agent_nostr_config VALUES ('agent-a','test-nostr-secret','[]','{}',1,'2026','','')", []).unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .unwrap();
    drop(conn);
    drop(opencrab_nostr_gateway::store::NostrStore::open(&nostr_path).unwrap());
    write_secret(
        &key_path,
        &base64::engine::general_purpose::STANDARD.encode([8u8; 32]),
    );
    let approval = Approval {
        version: 1,
        operation_id: "00000000-0000-4000-8000-000000000008".into(),
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
            source: "legacy-core:agent_nostr_config:agent-a".into(),
        }],
    };
    write_secure(&approval_path, &canonical::bytes(&approval).unwrap());
    let result = command::run_import(ImportArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        backup_dir: &backup_dir,
        report_path: &temp.path().join("report.json"),
        inputs: Inputs {
            paths: BTreeMap::from([(("nostr".into(), "nostr-primary".into()), nostr_path)]),
            master_keys: BTreeMap::from([("nostr".into(), key_path)]),
            credential_files: BTreeMap::new(),
        },
    });
    assert!(
        result.is_err(),
        "retained Discord credential/config must not be silently matched to a Nostr plan"
    );
    assert!(
        !backup_dir.exists(),
        "unmapped legacy config must fail before backup"
    );
}

#[test]
fn s8_disabled_discord_without_credential_imports_and_reruns_unconfigured() {
    assert_missing_credential_fixture("discord", false);
}

#[test]
fn s8_disabled_nostr_without_credential_imports_and_reruns_unconfigured() {
    assert_missing_credential_fixture("nostr", false);
}

#[test]
fn s8_enabled_discord_without_credential_still_fails_closed() {
    assert_missing_credential_fixture("discord", true);
}

fn assert_missing_credential_fixture(kind: &str, enabled: bool) {
    let temp = tempfile::tempdir().unwrap();
    let core_path = temp.path().join("core.db");
    let destination_path = temp.path().join("gateway.db");
    let key_path = temp.path().join("gateway.key");
    let approval_path = temp.path().join("approval.json");
    let backup_dir = temp.path().join("backups");
    let report_path = temp.path().join("report.json");
    seed_core(&core_path);
    let conn = Connection::open(&core_path).unwrap();
    conn.execute_batch(
        "DELETE FROM agent_discord_config; DELETE FROM trusted_users; DELETE FROM channel_config;",
    )
    .unwrap();
    conn.execute("UPDATE gate_instances SET enabled=?1", [enabled])
        .unwrap();
    if kind == "nostr" {
        let config_b64 = opencrab_nostr_gateway::config::canonicalize_config_b64(
            &base64::engine::general_purpose::STANDARD.encode(
                serde_json::to_vec(&serde_json::json!({
                    "relays": ["wss://example.invalid"],
                    "self_pubkey": "aa".repeat(32),
                    "name": "test-agent",
                    "access": {"owner": [], "co_agents": {}, "trusted_users": []}
                }))
                .unwrap(),
            ),
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
        conn.execute(
            "UPDATE gate_instances SET kind_id='nostr',config_b64=?1,config_digest=?2",
            params![config_b64, digest],
        )
        .unwrap();
    }
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .unwrap();
    drop(conn);
    if kind == "discord" {
        drop(opencrab_discord_gateway::store::DiscordStore::open(&destination_path).unwrap());
    } else {
        drop(opencrab_nostr_gateway::store::NostrStore::open(&destination_path).unwrap());
    }
    write_secret(
        &key_path,
        &base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
    );
    let path_id = format!("{kind}-primary");
    let approval = Approval {
        version: 1,
        operation_id: "00000000-0000-4000-8000-000000000008".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        core_user_version: 56,
        source_core_sha256: source::file_sha256(&core_path).unwrap(),
        destinations: vec![Destination {
            kind_id: kind.into(),
            path_id: path_id.clone(),
            schema: format!("s5-{kind}-v1"),
        }],
        identity_dispositions: vec![],
        channel_edges: vec![],
        watch_edges: vec![],
        credential_sources: vec![],
    };
    write_secure(&approval_path, &canonical::bytes(&approval).unwrap());
    let before = protected_counts(&core_path);
    let import = || {
        command::run_import(ImportArgs {
            core_path: &core_path,
            approval_path: &approval_path,
            backup_dir: &backup_dir,
            report_path: &report_path,
            inputs: Inputs {
                paths: BTreeMap::from([((kind.into(), path_id.clone()), destination_path.clone())]),
                master_keys: BTreeMap::from([(kind.into(), key_path.clone())]),
                credential_files: BTreeMap::new(),
            },
        })
    };
    if enabled {
        assert!(
            import()
                .unwrap_err()
                .to_string()
                .contains("credential source missing"),
            "an enabled instance cannot silently start without its credential"
        );
        assert!(
            !backup_dir.exists(),
            "missing enabled secret fails before backup"
        );
        return;
    }
    let report = import().expect("disabled instance without a secret must import unconfigured");
    assert_eq!(
        protected_counts(&core_path),
        before,
        "import must not rewrite core"
    );
    assert_eq!(report.destinations[0]["counts"]["credentials"], 0);
    assert_eq!(report.destinations[0]["credentials"], serde_json::json!([]));
    let conn = Connection::open(&destination_path).unwrap();
    let (enabled, envelope): (bool, String) = conn.query_row(
        "SELECT enabled,credential_envelope FROM instances WHERE instance_id='11111111-1111-4111-8111-111111111111'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert!(!enabled, "disabled instance must not start a child");
    assert_eq!(
        envelope, "",
        "absence is not an encrypted synthetic credential"
    );
    drop(conn);
    let rerun = import().expect("successful disabled import must rerun without a credential");
    assert_eq!(
        rerun.destination_manifest_sha256,
        report.destination_manifest_sha256
    );
}

fn seed_core(path: &std::path::Path) {
    let conn = opencrab_db::init_connection(path.to_str().unwrap()).unwrap();
    create_legacy_core_source_tables(&conn);
    conn.execute("INSERT INTO agents(agent_id,name,persona_name,instructions,created_at,updated_at) VALUES ('agent-a','A','A','','2026','2026')",[]).unwrap();
    let subject: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id='agent-a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute("INSERT INTO sessions(id,theme,created_at,updated_at) VALUES ('session-1','t','2026','2026')",[]).unwrap();
    conn.execute(
        "INSERT INTO agent_sessions(agent_id,session_id) VALUES ('agent-a','session-1')",
        [],
    )
    .unwrap();
    let config = serde_json::json!({"agent_id":"agent-a","self_bot_id":"99","access":{"owners":["42"],"co_agents":{},"trusted_users":[]},"system_reactions":{}});
    let raw_config_b64 =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&config).unwrap());
    let config_b64 =
        opencrab_discord_gateway::config::canonicalize_config_b64(&raw_config_b64).unwrap();
    let digest = format!(
        "{:x}",
        Sha256::digest(
            base64::engine::general_purpose::STANDARD
                .decode(&config_b64)
                .unwrap()
        )
    );
    conn.execute("INSERT INTO gate_instances(instance_id,kind_id,subject_id,revision,enabled,config_b64,config_digest,created_at,updated_at,association_grandfathered) VALUES (?1,'discord',?2,1,1,?3,?4,1,1,1)",params!["11111111-1111-4111-8111-111111111111",subject,config_b64,digest]).unwrap();
    conn.execute("INSERT INTO gate_bindings(binding_id,instance_id,address,created_at,session_id) VALUES ('binding-1','11111111-1111-4111-8111-111111111111','discord-agent-a-123-42',1,'session-1')",[]).unwrap();
    conn.execute("INSERT INTO channel_config(channel_id,agent_id,guild_id,channel_name,readable,writable,whitelisted,heartbeat_enabled,heartbeat_interval_secs,heartbeat_instructions,updated_at) VALUES ('42','agent-a','123','General',1,1,1,1,600,'Ping','2026-01-01T00:00:00Z')",[]).unwrap();
    conn.execute("INSERT INTO trusted_users VALUES ('tu-discord','42','agent-a','owner','owner','2026','Crab','discord')",[]).unwrap();
    conn.execute("INSERT INTO trusted_users VALUES ('tu-rest','rest-user','agent-a','owner','owner','2026','Rest','rest')",[]).unwrap();
    conn.execute("CREATE TABLE agent_discord_config(agent_id TEXT,bot_token TEXT,owner_discord_id TEXT,enabled INTEGER,updated_at TEXT,bot_user_id TEXT)",[]).unwrap();
    conn.execute(
        "INSERT INTO agent_discord_config VALUES ('agent-a','test-token','42',1,'2026','99')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO deliveries(delivery_id,binding_id,payload_json,state,error,created_at,updated_at) VALUES ('delivery-1','binding-1','{\"text\":\"hello\"}','delivered',NULL,1,1)",[]).unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .unwrap();
}
fn protected_counts(path: &std::path::Path) -> Vec<i64> {
    let conn = Connection::open(path).unwrap();
    [
        "agents",
        "sessions",
        "agent_sessions",
        "gate_instances",
        "gate_bindings",
        "deliveries",
        "trusted_users",
    ]
    .iter()
    .map(|table| {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    })
    .collect()
}
fn write_secret(path: &std::path::Path, text: &str) {
    write_secure(path, text.as_bytes())
}
fn write_secure(path: &std::path::Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn t(name: &str, value: &str) -> (String, Cell) {
    (name.into(), Cell::Text(value.into()))
}
fn i(name: &str, value: i64) -> (String, Cell) {
    (name.into(), Cell::Integer(value))
}
