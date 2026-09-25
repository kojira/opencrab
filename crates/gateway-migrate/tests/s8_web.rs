use base64::Engine as _;
use opencrab_gateway_migrate::{
    canonical,
    command::{self, ImportArgs, ProjectArgs},
    destination::Inputs,
    manifest::{Approval, CredentialSource, Destination, IdentityDisposition, IdentityEdge},
    source,
};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path};

const INSTANCE_ID: &str = "22222222-2222-4222-8222-222222222222";
const CREDENTIAL: &str = "web-test-credential";
const WEB_KEY: [u8; 32] = [9; 32];

#[test]
fn d_1006_web_01_credential_free_owner_preserves_unrelated_source_identity() {
    let temp = tempfile::tempdir().unwrap();
    let core_path = temp.path().join("core.db");
    let web_path = temp.path().join("web.db");
    let approval_path = temp.path().join("approval.json");
    let report_path = temp.path().join("report.json");
    let verification_path = temp.path().join("verification.json");
    let backup_dir = temp.path().join("backups");

    seed_web_core(&core_path);
    let store = opencrab_web_gateway::store::WebStore::open(&web_path).unwrap();
    store
        .upsert(
            INSTANCE_ID,
            "agent-web",
            1,
            "web-author",
            None,
            true,
            &WEB_KEY,
        )
        .unwrap();
    drop(store);
    Connection::open(&web_path).unwrap().execute(
        "INSERT INTO identity_projections(instance_id,role,external_id) VALUES (?1,'owner','web-local')",
        [INSTANCE_ID],
    ).unwrap();

    let rows = source::validate(&source::open_read_only(&core_path).unwrap()).unwrap();
    let row = rows
        .iter()
        .find(|row| row.table == "trusted_users")
        .unwrap();
    assert_eq!(row.text("user_id").unwrap(), "source-user-not-author");
    let approval = Approval {
        version: 1,
        operation_id: "00000000-0000-4000-8000-000000000009".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        core_user_version: 56,
        source_core_sha256: source::file_sha256(&core_path).unwrap(),
        destinations: vec![Destination {
            kind_id: "web".into(),
            path_id: "web-primary".into(),
            schema: "s5-web-v1".into(),
        }],
        identity_dispositions: vec![IdentityDisposition {
            source_fingerprint: row.fingerprint.clone(),
            edges: vec![IdentityEdge::Gateway {
                kind_id: "web".into(),
                instance_id: INSTANCE_ID.into(),
            }],
        }],
        channel_edges: vec![],
        watch_edges: vec![],
        credential_sources: vec![],
    };
    write_secure(&approval_path, &canonical::bytes(&approval).unwrap());
    let inputs = || Inputs {
        paths: BTreeMap::from([(("web".into(), "web-primary".into()), web_path.clone())]),
        master_keys: BTreeMap::new(),
        credential_files: BTreeMap::new(),
    };
    let import_args = || ImportArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        backup_dir: &backup_dir,
        report_path: &report_path,
        inputs: inputs(),
    };
    let project_args = || ProjectArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        import_report_path: &report_path,
        verification_path: &verification_path,
        destination_paths: BTreeMap::from([(
            ("web".into(), "web-primary".into()),
            web_path.clone(),
        )]),
    };
    let report = command::run_import(import_args())
        .expect("existing Web instance must accept an approved identity without a Web credential");
    let store = opencrab_web_gateway::store::WebStore::open(&web_path).unwrap();
    let owner_marker: i64 = Connection::open(&web_path).unwrap().query_row(
        "SELECT COUNT(*) FROM identity_projections WHERE instance_id=?1 AND role='owner' AND external_id='web-local'",
        [INSTANCE_ID], |r| r.get(0),
    ).unwrap();
    assert_eq!(
        owner_marker, 1,
        "historical Web admission must remain Owner without a bearer"
    );
    assert_eq!(
        store.get(INSTANCE_ID).unwrap().unwrap().author_id,
        "web-author"
    );
    assert!(store
        .get(INSTANCE_ID)
        .unwrap()
        .unwrap()
        .credential_envelope
        .is_none());
    let identity_count: i64 = Connection::open(&web_path).unwrap().query_row(
        "SELECT COUNT(*) FROM identity_projections WHERE instance_id=?1 AND role='trusted_user' AND external_id='source-user-not-author'",
        [INSTANCE_ID], |r| r.get(0),
    ).unwrap();
    assert_eq!(
        identity_count, 1,
        "the legacy external ID must be retained separately from the admitted bearer"
    );
    drop(store);
    command::run_project(project_args())
        .expect("Web-owned identity migration must project the core marker");
    command::run_import(import_args()).expect("the completed Web import must be idempotent");
    assert!(
        Connection::open(&web_path)
            .unwrap()
            .query_row(
                "SELECT credential_envelope FROM instances WHERE instance_id=?1",
                [INSTANCE_ID],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
            .is_none(),
        "rerun must not invent a Web credential"
    );
    assert_eq!(report.destinations[0]["counts"]["identity_projections"], 1);
}

#[test]
fn d_1006_web_01_co_agent_source_remains_inert() {
    assert_web_mapping_imports(&[("source-coagent", "co-agent")], "trusted_user");
}

#[test]
fn d_1006_web_01_multiple_source_ids_remain_inert() {
    assert_web_mapping_imports(
        &[("source-one", "user"), ("source-two", "user")],
        "trusted_user",
    );
}

#[test]
fn d_1006_web_01_source_role_does_not_select_admitted_owner() {
    assert_web_mapping_imports(&[("source-user", "user")], "owner");
}

#[test]
fn d_1006_web_01_rejects_reserved_web_local_identity_before_backup() {
    assert_web_mapping_refused(&[("web-local", "owner")], "trusted_user", None);
}

fn assert_web_mapping_imports(source_users: &[(&str, &str)], bearer_role: &str) {
    assert_web_mapping(source_users, bearer_role, true, None);
}

fn assert_web_mapping_refused(
    source_users: &[(&str, &str)],
    bearer_role: &str,
    expected_error: Option<&str>,
) {
    assert_web_mapping(source_users, bearer_role, false, expected_error);
}

fn assert_web_mapping(
    source_users: &[(&str, &str)],
    bearer_role: &str,
    should_import: bool,
    expected_error: Option<&str>,
) {
    let temp = tempfile::tempdir().unwrap();
    let core_path = temp.path().join("core.db");
    let web_path = temp.path().join("web.db");
    let approval_path = temp.path().join("approval.json");
    let report_path = temp.path().join("report.json");
    let backup_dir = temp.path().join("backups");
    let key_path = temp.path().join("web.key");
    seed_web_core(&core_path);
    let conn = Connection::open(&core_path).unwrap();
    conn.execute("DELETE FROM trusted_users", []).unwrap();
    for (index, (user_id, permission)) in source_users.iter().enumerate() {
        conn.execute(
            "INSERT INTO trusted_users VALUES (?1,?2,'agent-web',?3,'owner','2026','Web','web')",
            params![format!("tu-web-{index}"), user_id, permission],
        )
        .unwrap();
    }
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .unwrap();
    drop(conn);
    let store = opencrab_web_gateway::store::WebStore::open(&web_path).unwrap();
    store
        .upsert(
            INSTANCE_ID,
            "agent-web",
            1,
            "web-author",
            Some(CREDENTIAL),
            true,
            &WEB_KEY,
        )
        .unwrap();
    store.set_caller_role(INSTANCE_ID, bearer_role).unwrap();
    assert_eq!(store.caller_role(INSTANCE_ID).unwrap(), bearer_role);
    drop(store);
    Connection::open(&web_path).unwrap().execute(
        "INSERT INTO identity_projections(instance_id,role,external_id) VALUES (?1,'owner','web-local')",
        [INSTANCE_ID],
    ).unwrap();
    let before_web = fs::read(&web_path).unwrap();
    let before_core = fs::read(&core_path).unwrap();
    write_secure(
        &key_path,
        base64::engine::general_purpose::STANDARD
            .encode(WEB_KEY)
            .as_bytes(),
    );
    let rows = source::validate(&source::open_read_only(&core_path).unwrap()).unwrap();
    let mut dispositions = rows
        .iter()
        .filter(|row| row.table == "trusted_users")
        .map(|row| IdentityDisposition {
            source_fingerprint: row.fingerprint.clone(),
            edges: vec![IdentityEdge::Gateway {
                kind_id: "web".into(),
                instance_id: INSTANCE_ID.into(),
            }],
        })
        .collect::<Vec<_>>();
    assert_eq!(dispositions.len(), source_users.len());
    dispositions.sort_by(|a, b| a.source_fingerprint.cmp(&b.source_fingerprint));
    let approval = Approval {
        version: 1,
        operation_id: "00000000-0000-4000-8000-000000000010".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        core_user_version: 56,
        source_core_sha256: source::file_sha256(&core_path).unwrap(),
        destinations: vec![Destination {
            kind_id: "web".into(),
            path_id: "web-primary".into(),
            schema: "s5-web-v1".into(),
        }],
        identity_dispositions: dispositions,
        channel_edges: vec![],
        watch_edges: vec![],
        credential_sources: vec![CredentialSource {
            instance_id: INSTANCE_ID.into(),
            source: format!("existing-destination:web:{INSTANCE_ID}"),
        }],
    };
    write_secure(&approval_path, &canonical::bytes(&approval).unwrap());
    let outcome = command::run_import(ImportArgs {
        core_path: &core_path,
        approval_path: &approval_path,
        backup_dir: &backup_dir,
        report_path: &report_path,
        inputs: Inputs {
            paths: BTreeMap::from([(("web".into(), "web-primary".into()), web_path.clone())]),
            master_keys: BTreeMap::from([("web".into(), key_path)]),
            credential_files: BTreeMap::new(),
        },
    });
    if should_import {
        outcome.expect("explicit Web source rows are inert and must not change Owner admission");
        let conn = Connection::open(&web_path).unwrap();
        for (user_id, permission) in source_users {
            let role = match *permission {
                "owner" => "owner",
                "co-agent" => "co_agent",
                _ => "trusted_user",
            };
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM identity_projections WHERE instance_id=?1 AND role=?2 AND external_id=?3",
                params![INSTANCE_ID, role, user_id], |r| r.get(0),
            ).unwrap();
            assert_eq!(
                count, 1,
                "source identity {user_id} must remain separate from Web admission"
            );
        }
        let marker: i64 = conn.query_row(
            "SELECT COUNT(*) FROM identity_projections WHERE instance_id=?1 AND role='owner' AND external_id='web-local'",
            [INSTANCE_ID], |r| r.get(0),
        ).unwrap();
        assert_eq!(marker, 1);
        assert_eq!(
            fs::read(&core_path).unwrap(),
            before_core,
            "import must retain source identity IDs"
        );
    } else {
        let error = outcome.expect_err("reserved Web identity collision was accepted");
        if let Some(expected) = expected_error {
            assert!(
                format!("{error:#}").contains(expected),
                "wrong refusal: {error:#}"
            );
        }
        assert!(
            !backup_dir.exists(),
            "mapping must fail before matched backup: {error:#}"
        );
        assert!(
            !report_path.exists(),
            "mapping must not produce an import report"
        );
        assert_eq!(fs::read(&web_path).unwrap(), before_web);
        assert_eq!(fs::read(&core_path).unwrap(), before_core);
    }
}

fn seed_web_core(path: &Path) {
    let conn = opencrab_db::init_connection(path.to_str().unwrap()).unwrap();
    conn.execute("INSERT INTO agents(agent_id,name,persona_name,instructions,created_at,updated_at) VALUES ('agent-web','Web','Web','','2026','2026')", []).unwrap();
    let subject: i64 = conn
        .query_row(
            "SELECT subject_id FROM agents WHERE agent_id='agent-web'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let config = serde_json::json!({"author_id":"web-author"});
    let raw = serde_json::to_vec(&config).unwrap();
    let config_b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
    let digest = format!("{:x}", Sha256::digest(&raw));
    conn.execute("INSERT INTO gate_instances(instance_id,kind_id,subject_id,revision,enabled,config_b64,config_digest,created_at,updated_at,association_grandfathered) VALUES (?1,'web',?2,1,1,?3,?4,1,1,1)", params![INSTANCE_ID,subject,config_b64,digest]).unwrap();
    conn.execute("INSERT INTO trusted_users VALUES ('tu-web','source-user-not-author','agent-web','user','owner','2026','Web','web')", []).unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .unwrap();
}

fn write_secure(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
