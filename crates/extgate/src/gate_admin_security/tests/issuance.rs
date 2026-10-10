//! Issue #1070: principals and grants issued while core runs, without a restart.

use super::*;

const FAR: i64 = 4_000_000_000_000_000_000;
const NAMESPACE: &str = "6f1e8a52-4c1d-4b4e-9d2a-3f0c7b5e9a11";

struct Running {
    state: std::sync::Arc<crate::registry::ExtgateState>,
    app: axum::Router,
    original: CredentialManifest,
}

/// A "running core": bootstrapped once with an exact principal over subject 1, router built.
fn running_core() -> Running {
    let db = opencrab_db::Db::memory().unwrap();
    let original = {
        let mut credential = manifest([41; 32], "bootstrap", FAR);
        credential.operations = Operation::ALL.into_iter().collect();
        credential
    };
    {
        let mut conn = db.lock().unwrap();
        conn.execute_batch(
            "INSERT INTO agents(agent_id,name,persona_name,subject_id) VALUES ('agent-a','a','a',1);
             INSERT INTO agents(agent_id,name,persona_name,subject_id) VALUES ('agent-ext','t','t',2);",
        )
        .unwrap();
        bootstrap(&mut conn, &original, 100).unwrap();
    }
    let state = std::sync::Arc::new(crate::registry::ExtgateState::new_protected(db));
    let app = crate::admin::admin_router(state.clone());
    Running {
        state,
        app,
        original,
    }
}

fn ext_request() -> PrincipalRequest {
    PrincipalRequest {
        principal_id: "ext".to_owned(),
        operations: [
            Operation::InstanceRead,
            Operation::InstancePut,
            Operation::BindingPut,
        ]
        .into_iter()
        .collect(),
        subject_ids: [2].into_iter().collect(),
        scope: PrincipalScope::CreationNamespace(Uuid::parse_str(NAMESPACE).unwrap()),
        expires_at: 1_000 + 86_400 * 1_000_000_000,
    }
}

fn instance_body(subject_id: i64, grant: Option<&str>) -> String {
    let mut body = serde_json::json!({
        "kind_id": "opaque",
        "subject_id": subject_id,
        "enabled": true,
        "config_b64": "",
    });
    if let Some(grant) = grant {
        body["subject_grant"] = serde_json::Value::from(grant);
    }
    body.to_string()
}

fn derived(agent_id: &str) -> Uuid {
    namespace_instance_id(&Uuid::parse_str(NAMESPACE).unwrap(), agent_id)
}

async fn status(app: &axum::Router, method: &str, uri: &str, bearer: &str, body: &str) -> u16 {
    super::handler_followup::protected_request(app, method, uri, Some(bearer), body)
        .await
        .status()
        .as_u16()
}

#[tokio::test]
async fn issued_namespace_principal_provisions_new_instance_without_restart() {
    let core = running_core();
    let now = crate::ids::now_nanos();
    let mut request = ext_request();
    request.expires_at = now + 86_400 * 1_000_000_000;
    let bearer = {
        let mut conn = core.state.db.lock().unwrap();
        issue_principal(&mut conn, &request, now).unwrap()
    };
    let bearer = format!("Bearer {}", bearer.expose_secret());
    let instance = format!("/api/gate-instances/{}", derived("agent-ext"));

    // Without a grant the new association is refused even for an in-scope principal.
    assert_eq!(
        status(
            &core.app,
            "PUT",
            &instance,
            &bearer,
            &instance_body(2, None)
        )
        .await,
        409
    );

    let grant = {
        let mut conn = core.state.db.lock().unwrap();
        issue_subject_grant(&mut conn, "agent-ext", 2, 600 * 1_000_000_000, now).unwrap()
    };
    let body = instance_body(2, Some(grant.expose_secret()));
    assert_eq!(
        status(&core.app, "PUT", &instance, &bearer, &body).await,
        201
    );
    // Byte-identical retry needs no second grant; the grant itself is single-use.
    assert_eq!(
        status(&core.app, "PUT", &instance, &bearer, &body).await,
        200
    );

    let binding_id = Uuid::new_v4();
    let binding = serde_json::json!({
        "instance_id": derived("agent-ext").to_string(),
        "address": "ext:room",
        "session": {"session_id": format!("extgate-{binding_id}"), "title": "ext"},
    })
    .to_string();
    let binding_uri = format!("/api/gate-bindings/{binding_id}");
    assert_eq!(
        status(&core.app, "PUT", &binding_uri, &bearer, &binding).await,
        201
    );
    assert_eq!(status(&core.app, "GET", &instance, &bearer, "").await, 200);

    let conn = core.state.db.lock().unwrap();
    let audited: i64 = conn
        .query_row(
            "SELECT count(*) FROM gate_admin_request_audit
             WHERE principal_id='ext' AND result_class='succeeded'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audited, 4, "every successful provisioning step is audited");
}

#[tokio::test]
async fn unauthorized_provisioning_paths_are_rejected() {
    let core = running_core();
    let now = crate::ids::now_nanos();
    let mut request = ext_request();
    request.expires_at = now + 86_400 * 1_000_000_000;
    let ext = {
        let mut conn = core.state.db.lock().unwrap();
        issue_principal(&mut conn, &request, now).unwrap()
    };
    let ext = format!("Bearer {}", ext.expose_secret());
    let original = format!("Bearer {}", URL_SAFE_NO_PAD.encode([41_u8; 32]));
    let grant = |agent: &str, subject: i64| {
        let mut conn = core.state.db.lock().unwrap();
        issue_subject_grant(&mut conn, agent, subject, 600 * 1_000_000_000, now)
    };
    let ext_uri = format!("/api/gate-instances/{}", derived("agent-ext"));

    // The bootstrap principal is exact-scoped to subject 1: it cannot reach subject 2.
    let body = instance_body(2, Some(grant("agent-ext", 2).unwrap().expose_secret()));
    assert_eq!(
        status(&core.app, "PUT", &ext_uri, &original, &body).await,
        401
    );
    // The new principal cannot pick an instance ID outside its namespace derivation.
    let random = format!("/api/gate-instances/{}", Uuid::new_v4());
    assert_eq!(status(&core.app, "PUT", &random, &ext, &body).await, 401);
    // Nor reach another agent's subject, even with that agent's derived ID and a grant.
    let other = format!("/api/gate-instances/{}", derived("agent-a"));
    let body_a = instance_body(1, Some(grant("agent-a", 1).unwrap().expose_secret()));
    assert_eq!(status(&core.app, "PUT", &other, &ext, &body_a).await, 401);
    // Nor use an operation it was not granted.
    assert_eq!(status(&core.app, "DELETE", &ext_uri, &ext, "").await, 401);

    // Grants are pair-bound and short-lived.
    assert!(grant("agent-a", 2).is_err());
    let mut conn = core.state.db.lock().unwrap();
    let too_long = MAX_SUBJECT_GRANT_TTL_NANOS + 1;
    assert!(issue_subject_grant(&mut conn, "agent-ext", 2, too_long, now).is_err());

    // Principals cannot pre-claim absent or tombstoned subjects, or reuse an ID.
    let mut absent = ext_request();
    absent.principal_id = "absent".to_owned();
    absent.subject_ids = [99].into_iter().collect();
    absent.expires_at = now + 1_000;
    assert!(matches!(
        issue_principal(&mut conn, &absent, now),
        Err(SecurityError::Conflict)
    ));
    let mut duplicate = ext_request();
    duplicate.expires_at = now + 1_000;
    assert!(matches!(
        issue_principal(&mut conn, &duplicate, now),
        Err(SecurityError::Conflict)
    ));
    let mut forever = ext_request();
    forever.principal_id = "forever".to_owned();
    forever.expires_at = FAR;
    assert!(issue_principal(&mut conn, &forever, now).is_err());

    // Revocation takes effect on the next request without a restart.
    revoke(&conn, "ext", now).unwrap();
    drop(conn);
    assert_eq!(status(&core.app, "GET", &ext_uri, &ext, "").await, 401);
}

#[test]
fn extra_issued_principal_keeps_bootstrap_restart_verification_intact() {
    let core = running_core();
    let mut conn = core.state.db.lock().unwrap();
    let mut request = ext_request();
    request.expires_at = 1_000_000;
    issue_principal(&mut conn, &request, 200).unwrap();
    let restart = bootstrap(&mut conn, &core.original, 300).unwrap();
    assert_eq!(
        restart,
        BootstrapOutcome {
            created: false,
            scanned_principals: 2
        }
    );
}
