//! D-1097: `POST /api/gate-subject-grants` lets a principal that may create instances obtain the
//! single-use subject grant for an in-scope subject, so a gateway can create a new instance
//! without the operator CLI.

use super::*;

const NAMESPACE: &str = "0c9a7e51-2b3d-4f6a-8e1c-5d7b9f2a4c63";
const GRANTS: &str = "/api/gate-subject-grants";

struct Core {
    state: std::sync::Arc<crate::registry::ExtgateState>,
    app: axum::Router,
}

/// A running core with agent-a (subject 1) and agent-b (subject 2), and a creation-namespace
/// principal `creator` over subject 1 only. `operations` is that principal's operation set.
fn core_with_creator(operations: &[Operation]) -> (Core, String) {
    let db = opencrab_db::Db::memory().unwrap();
    {
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "INSERT INTO agents(agent_id,name,persona_name,subject_id) VALUES ('agent-a','a','a',1);
             INSERT INTO agents(agent_id,name,persona_name,subject_id) VALUES ('agent-b','b','b',2);",
        )
        .unwrap();
    }
    let state = std::sync::Arc::new(crate::registry::ExtgateState::new_protected(db));
    let now = crate::ids::now_nanos();
    let request = PrincipalRequest {
        principal_id: "creator".to_owned(),
        operations: operations.iter().copied().collect(),
        subject_ids: [1].into_iter().collect(),
        scope: PrincipalScope::CreationNamespace(Uuid::parse_str(NAMESPACE).unwrap()),
        expires_at: now + 86_400 * 1_000_000_000,
    };
    let bearer = {
        let mut conn = state.db.lock().unwrap();
        issue_principal(&mut conn, &request, now).unwrap()
    };
    let bearer = format!("Bearer {}", bearer.expose_secret());
    let app = crate::admin::admin_router(state.clone());
    (Core { state, app }, bearer)
}

fn derived(agent_id: &str) -> Uuid {
    namespace_instance_id(&Uuid::parse_str(NAMESPACE).unwrap(), agent_id)
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    use http_body_util::BodyExt as _;
    let response =
        super::handler_followup::protected_request(app, method, uri, Some(bearer), body).await;
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

fn create_body(subject_id: i64, grant: &str) -> String {
    serde_json::json!({
        "kind_id": "opaque",
        "subject_id": subject_id,
        "enabled": true,
        "config_b64": "",
        "subject_grant": grant,
    })
    .to_string()
}

fn audit_rows(core: &Core) -> Vec<(String, Option<i64>, String)> {
    let conn = core.state.db.lock().unwrap();
    let mut statement = conn
        .prepare(
            "SELECT operation, authorized_subject_id, result_class
             FROM gate_admin_request_audit WHERE principal_id='creator' ORDER BY attempted_at, rowid",
        )
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[tokio::test]
async fn subject_grant_route_issues_a_grant_that_creates_the_namespace_instance() {
    let (core, bearer) = core_with_creator(&[Operation::InstancePut]);
    let (status, body) = request(
        &core.app,
        "POST",
        GRANTS,
        &bearer,
        r#"{"agent_id":"agent-a"}"#,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["subject_id"], 1);
    let grant = body["grant"].as_str().expect("grant");
    assert!(!grant.is_empty());

    let instance = format!("/api/gate-instances/{}", derived("agent-a"));
    let (created, _) = request(&core.app, "PUT", &instance, &bearer, &create_body(1, grant)).await;
    assert_eq!(created, 201);

    let audit = audit_rows(&core);
    assert_eq!(
        audit[0],
        ("instance.put".to_owned(), Some(1), "succeeded".to_owned()),
        "the grant issue is audited as an authorized instance.put for the subject"
    );
}

#[tokio::test]
async fn subject_grant_route_refuses_a_subject_outside_the_principal_scope() {
    let (core, bearer) = core_with_creator(&[Operation::InstancePut]);
    let (status, _) = request(
        &core.app,
        "POST",
        GRANTS,
        &bearer,
        r#"{"agent_id":"agent-b"}"#,
    )
    .await;
    assert_eq!(status, 401);
    let conn = core.state.db.lock().unwrap();
    let grants: i64 = conn
        .query_row(
            "SELECT count(*) FROM subject_association_grants",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        grants, 0,
        "no grant row is written for an out-of-scope subject"
    );
}

#[tokio::test]
async fn subject_grant_route_refuses_a_principal_without_instance_put() {
    let (core, bearer) = core_with_creator(&[Operation::InstanceRead, Operation::BindingPut]);
    let (status, _) = request(
        &core.app,
        "POST",
        GRANTS,
        &bearer,
        r#"{"agent_id":"agent-a"}"#,
    )
    .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn subject_grant_route_grant_is_single_use() {
    let (core, bearer) = core_with_creator(&[Operation::InstancePut, Operation::InstanceDelete]);
    let (_, body) = request(
        &core.app,
        "POST",
        GRANTS,
        &bearer,
        r#"{"agent_id":"agent-a"}"#,
    )
    .await;
    let grant = body["grant"].as_str().expect("grant").to_owned();
    let instance = format!("/api/gate-instances/{}", derived("agent-a"));
    let (created, _) = request(
        &core.app,
        "PUT",
        &instance,
        &bearer,
        &create_body(1, &grant),
    )
    .await;
    assert_eq!(created, 201);

    // Delete the instance and try to create it again with the same grant.
    let (deleted, _) = request(&core.app, "DELETE", &instance, &bearer, "").await;
    assert_eq!(deleted, 200);
    let conn = core.state.db.lock().unwrap();
    conn.execute(
        "DELETE FROM gate_instances WHERE instance_id=?1",
        [derived("agent-a").to_string()],
    )
    .unwrap();
    drop(conn);
    let (reused, _) = request(
        &core.app,
        "PUT",
        &instance,
        &bearer,
        &create_body(1, &grant),
    )
    .await;
    assert_eq!(
        reused, 409,
        "a consumed grant cannot create another association"
    );
}

#[tokio::test]
async fn subject_grant_route_rejects_unknown_agents_and_extra_fields() {
    let (core, bearer) = core_with_creator(&[Operation::InstancePut]);
    let (unknown, _) = request(
        &core.app,
        "POST",
        GRANTS,
        &bearer,
        r#"{"agent_id":"agent-missing"}"#,
    )
    .await;
    assert_eq!(unknown, 404);
    let (extra, _) = request(
        &core.app,
        "POST",
        GRANTS,
        &bearer,
        r#"{"agent_id":"agent-a","subject_id":2}"#,
    )
    .await;
    assert_eq!(extra, 400);
}
