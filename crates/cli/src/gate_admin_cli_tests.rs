//! Issue #1070: the operator CLI issues a principal and a grant against a live core
//! database, and a running gate-admin socket honors them without a restart.

use super::*;
use std::os::unix::fs::PermissionsExt as _;

use opencrab_gate_client::admin::{DesiredInstance, GateAdminClient};

const NAMESPACE: &str = "6f1e8a52-4c1d-4b4e-9d2a-3f0c7b5e9a11";

struct Core {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db_path: String,
    state: std::sync::Arc<opencrab_extgate::registry::ExtgateState>,
}

/// A migrated core database with two agents and no gate-admin principal yet.
fn core() -> Core {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = dir.path().canonicalize().unwrap();
    let db_path = root.join("core.db").to_str().unwrap().to_owned();
    let db = opencrab_db::Db::open(&db_path).unwrap();
    db.lock()
        .unwrap()
        .execute_batch(
            "INSERT INTO agents(agent_id,name,persona_name,subject_id) VALUES ('agent-a','a','a',1);
             INSERT INTO agents(agent_id,name,persona_name,subject_id) VALUES ('agent-town','t','t',2);",
        )
        .unwrap();
    let state = std::sync::Arc::new(opencrab_extgate::registry::ExtgateState::new_protected(db));
    Core {
        _dir: dir,
        root,
        db_path,
        state,
    }
}

fn cli(core: &Core, args: &[&str]) -> Result<String> {
    let mut raw = vec!["--db".to_owned(), core.db_path.clone()];
    raw.extend(args.iter().map(|arg| (*arg).to_owned()));
    run(&raw)
}

fn expires_in_days(days: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::days(days)).to_rfc3339()
}

fn issue_town(core: &Core, out: &Path) -> Result<String> {
    let expires = expires_in_days(30);
    cli(
        core,
        &[
            "principal-issue",
            "--principal-id",
            "crab-town",
            "--operation",
            "instance.read",
            "--operation",
            "instance.put",
            "--operation",
            "binding.put",
            "--subject",
            "2",
            "--creation-namespace",
            NAMESPACE,
            "--expires-at",
            &expires,
            "--credential-out",
            out.to_str().unwrap(),
        ],
    )
}

fn grant(core: &Core, agent: &str, subject: &str, ttl: &str, out: &Path) -> Result<String> {
    cli(
        core,
        &[
            "grant-issue",
            "--agent-id",
            agent,
            "--subject",
            subject,
            "--ttl-secs",
            ttl,
            "--grant-out",
            out.to_str().unwrap(),
        ],
    )
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn cli_issued_principal_and_grant_provision_instance_over_running_admin_socket() {
    let core = core();
    let socket = core.root.join("admin.sock");
    let prepared =
        opencrab_extgate::admin_socket::prepare_admin_socket(&socket, unsafe { libc::geteuid() })
            .unwrap();
    let (listener, _cleanup) = prepared.into_parts();
    let app = opencrab_extgate::admin::admin_router(core.state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    // The socket is already serving; the principal is added afterwards, with no restart.
    let credential = core.root.join("crab-town.credential");
    let message = issue_town(&core, &credential).unwrap();
    assert!(message.contains("crab-town"));
    assert_eq!(mode(&credential), 0o600);
    let secret = std::fs::read_to_string(&credential).unwrap();
    assert!(
        !message.contains(secret.trim()),
        "bearer must not reach stdout"
    );

    let instance_id = instance_id_for("agent-town");
    let client = GateAdminClient::from_credential_file(socket.clone(), &credential).unwrap();
    assert!(client.get_instance(&instance_id).await.unwrap().is_none());
    fn desired(grant: Option<&str>) -> DesiredInstance<'_> {
        DesiredInstance {
            kind_id: "opaque",
            subject_id: 2,
            enabled: true,
            config_b64: "",
            subject_grant: grant,
        }
    }
    // Without a grant, a new association is refused even for an in-scope principal.
    assert!(client
        .put_instance(&instance_id, &desired(None))
        .await
        .is_err());

    let grant_file = core.root.join("crab-town.grant");
    grant(&core, "agent-town", "2", "600", &grant_file).unwrap();
    assert_eq!(mode(&grant_file), 0o600);
    let token = std::fs::read_to_string(&grant_file).unwrap();
    let created = client
        .put_instance(&instance_id, &desired(Some(token.trim())))
        .await
        .unwrap();
    assert_eq!(created.subject_id, 2);

    let binding_id = uuid::Uuid::new_v4().to_string();
    let session_id = format!("extgate-{binding_id}");
    let binding = client
        .put_binding(&binding_id, &instance_id, "town:room", &session_id, "town")
        .await
        .unwrap();
    assert_eq!(binding.address, "town:room");

    // Revocation through the CLI takes effect on the next request, again without restart.
    cli(&core, &["principal-revoke", "--principal-id", "crab-town"]).unwrap();
    assert!(client.get_instance(&instance_id).await.is_err());
    server.abort();
}

fn instance_id_for(agent: &str) -> String {
    run(&[
        "instance-id".to_owned(),
        "--creation-namespace".to_owned(),
        NAMESPACE.to_owned(),
        "--agent-id".to_owned(),
        agent.to_owned(),
    ])
    .unwrap()
}

#[test]
fn cli_refuses_unscoped_or_overreaching_issuance() {
    let core = core();
    let out = core.root.join("x.credential");
    let expires = expires_in_days(30);
    let base = |extra: &[&str]| {
        let mut args = vec![
            "principal-issue",
            "--principal-id",
            "p",
            "--expires-at",
            expires.as_str(),
            "--credential-out",
            out.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        cli(&core, &args)
    };
    // No operation, no subject, no target scope, both scopes, unknown/wildcard operation.
    for extra in [
        &["--subject", "2", "--creation-namespace", NAMESPACE][..],
        &[
            "--operation",
            "instance.read",
            "--creation-namespace",
            NAMESPACE,
        ][..],
        &["--operation", "instance.read", "--subject", "2"][..],
        &[
            "--operation",
            "instance.read",
            "--subject",
            "2",
            "--creation-namespace",
            NAMESPACE,
            "--instance",
            NAMESPACE,
        ][..],
        &[
            "--operation",
            "*",
            "--subject",
            "2",
            "--creation-namespace",
            NAMESPACE,
        ][..],
        // A subject that does not exist yet must not be pre-claimed.
        &[
            "--operation",
            "instance.read",
            "--subject",
            "3",
            "--creation-namespace",
            NAMESPACE,
        ][..],
    ] {
        assert!(base(extra).is_err(), "{extra:?}");
        assert!(
            !out.exists(),
            "a refused issuance leaves no credential file"
        );
    }

    // Grants: pair mismatch and TTL above one hour are refused, and leave no file.
    let grant_out = core.root.join("g.grant");
    assert!(grant(&core, "agent-a", "2", "600", &grant_out).is_err());
    assert!(grant(&core, "agent-town", "2", "3601", &grant_out).is_err());
    assert!(!grant_out.exists());

    // An existing path is never overwritten (and no principal is created).
    std::fs::write(&out, "keep").unwrap();
    assert!(issue_town(&core, &out).is_err());
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "keep");
    let count: i64 = core
        .state
        .db
        .lock()
        .unwrap()
        .query_row("SELECT count(*) FROM gate_admin_principals", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn cli_never_creates_or_migrates_a_database() {
    let core = core();
    let missing = core.root.join("absent.db");
    let raw = |db: &Path| {
        run(&[
            "--db".to_owned(),
            db.to_str().unwrap().to_owned(),
            "principal-revoke".to_owned(),
            "--principal-id".to_owned(),
            "p".to_owned(),
        ])
    };
    assert!(raw(&missing).is_err());
    assert!(!missing.exists());

    let stale = core.root.join("stale.db");
    let conn = Connection::open(&stale).unwrap();
    conn.execute_batch("PRAGMA user_version=1;").unwrap();
    drop(conn);
    let error = raw(&stale).unwrap_err().to_string();
    assert!(error.contains("schema version"), "{error}");
}
