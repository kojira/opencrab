use super::*;

#[tokio::test]
async fn protected_router_serves_all_six_scoped_operations_with_database_credential() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let db = opencrab_db::Db::memory().unwrap();
    let token = [13_u8; 32];
    {
        let mut conn = db.lock().unwrap();
        conn.execute("INSERT INTO agents(agent_id,name,persona_name,subject_id) VALUES ('agent-a','a','a',1)", []).unwrap();
        let mut credential = manifest(token, "protected", 4_000_000_000_000_000_000);
        credential.operations = Operation::ALL.into_iter().collect();
        bootstrap(&mut conn, &credential, 100).unwrap();
    }
    let state = std::sync::Arc::new(crate::registry::ExtgateState::new_protected(db));
    let app = crate::admin::admin_router(state.clone());
    let bearer = format!("Bearer {}", URL_SAFE_NO_PAD.encode(token));
    let instance = "/api/gate-instances/00000000-0000-0000-0000-000000000000";
    let binding = "/api/gate-bindings/00000000-0000-0000-0000-000000000002";
    let grant = issue_test_subject_grant(&state);
    let cases = [
        (
            "PUT",
            instance.to_owned(),
            serde_json::json!({
                "kind_id": "opaque",
                "subject_id": 1,
                "enabled": true,
                "config_b64": "",
                "subject_grant": grant,
            })
            .to_string(),
            201,
        ),
        ("GET", instance.to_owned(), String::new(), 200),
        (
            "POST",
            format!("{instance}/revisions"),
            r#"{"expected_revision":1,"enabled":true,"config_b64":""}"#.to_owned(),
            201,
        ),
        (
            "PUT",
            binding.to_owned(),
            r#"{"instance_id":"00000000-0000-0000-0000-000000000000","address":"room","session":{"session_id":"extgate-00000000-0000-0000-0000-000000000002","title":"room"}}"#.to_owned(),
            201,
        ),
        ("DELETE", binding.to_owned(), String::new(), 200),
        ("DELETE", instance.to_owned(), String::new(), 200),
    ];
    for (method, uri, body, expected) in cases {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", &bearer)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
    let conn = state.db.lock().unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM gate_admin_request_audit", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        6
    );
}

pub(super) async fn protected_request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: &str,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let mut request = Request::builder().method(method).uri(uri);
    if let Some(bearer) = bearer {
        request = request.header("authorization", bearer);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap()
}

fn issue_test_subject_grant(state: &crate::registry::ExtgateState) -> String {
    let mut conn = state.db.lock().unwrap();
    opencrab_db::queries::issue_subject_association_grant(&mut conn, "agent-a", 1, i64::MAX, 100)
        .unwrap()
}

fn protected_state(token: [u8; 32]) -> std::sync::Arc<crate::registry::ExtgateState> {
    let db = opencrab_db::Db::memory().unwrap();
    {
        let mut conn = db.lock().unwrap();
        conn.execute("INSERT INTO agents(agent_id,name,persona_name,subject_id) VALUES ('agent-a','a','a',1)", []).unwrap();
        let mut credential = manifest(token, "protected", 4_000_000_000_000_000_000);
        credential.operations = Operation::ALL.into_iter().collect();
        bootstrap(&mut conn, &credential, 100).unwrap();
    }
    std::sync::Arc::new(crate::registry::ExtgateState::new_protected(db))
}

#[tokio::test]
async fn handlers_authenticate_before_parsing_or_lookup_and_audit_denials_and_authorized_errors() {
    let token = [31_u8; 32];
    let state = protected_state(token);
    let app = crate::admin::admin_router(state.clone());
    let bearer = format!("Bearer {}", URL_SAFE_NO_PAD.encode(token));
    let instance = "/api/gate-instances/00000000-0000-0000-0000-000000000000";

    let denied = protected_request(&app, "PUT", instance, None, "not-json").await;
    assert_eq!(denied.status(), axum::http::StatusCode::UNAUTHORIZED);

    let malformed = protected_request(&app, "PUT", instance, Some(&bearer), "not-json").await;
    assert_eq!(malformed.status(), axum::http::StatusCode::BAD_REQUEST);

    let missing = protected_request(&app, "GET", instance, Some(&bearer), "").await;
    assert_eq!(missing.status(), axum::http::StatusCode::NOT_FOUND);

    let conn = state.db.lock().unwrap();
    let rows = conn
        .prepare(
            "SELECT principal_id, authorized_subject_id, authorized_instance_id, result_class
             FROM gate_admin_request_audit ORDER BY attempted_at, rowid",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 3, "every handler outcome must be audited");
    assert_eq!(rows[0], (None, None, None, "unauthorized".to_owned()));
    assert_eq!(rows[1].0.as_deref(), Some("protected"));
    assert_eq!(rows[1].1, None);
    assert_eq!(rows[1].2, None);
    assert_eq!(rows[1].3, "bad_request");
    assert_eq!(rows[2].0.as_deref(), Some("protected"));
    assert_eq!(rows[2].3, "not_found");
}

#[tokio::test]
async fn handler_mutation_and_required_audit_commit_atomically_and_conflicts_are_audited() {
    let token = [32_u8; 32];
    let state = protected_state(token);
    let app = crate::admin::admin_router(state.clone());
    let bearer = format!("Bearer {}", URL_SAFE_NO_PAD.encode(token));
    let instance = "/api/gate-instances/00000000-0000-0000-0000-000000000000";
    let body = serde_json::json!({
        "kind_id": "opaque",
        "subject_id": 1,
        "enabled": true,
        "config_b64": "",
        "subject_grant": issue_test_subject_grant(&state),
    })
    .to_string();
    {
        let conn = state.db.lock().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_handler_success_audit
             BEFORE INSERT ON gate_admin_request_audit
             WHEN NEW.result_class='succeeded'
             BEGIN SELECT RAISE(ABORT, 'injected audit failure'); END;",
        )
        .unwrap();
    }

    let failed = protected_request(&app, "PUT", instance, Some(&bearer), &body).await;
    assert_eq!(
        failed.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    {
        let conn = state.db.lock().unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM gate_instances", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "audit failure must roll back the handler's domain mutation",
        );
        conn.execute_batch("DROP TRIGGER fail_handler_success_audit;")
            .unwrap();
    }

    let created = protected_request(&app, "PUT", instance, Some(&bearer), &body).await;
    assert_eq!(created.status(), axum::http::StatusCode::CREATED);
    let conflict_body = r#"{"kind_id":"different","subject_id":1,"enabled":true,"config_b64":""}"#;
    let conflict = protected_request(&app, "PUT", instance, Some(&bearer), conflict_body).await;
    assert_eq!(conflict.status(), axum::http::StatusCode::CONFLICT);

    let conn = state.db.lock().unwrap();
    let classes = conn
        .prepare("SELECT result_class FROM gate_admin_request_audit ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(classes, ["succeeded", "conflict"]);
}

#[tokio::test]
async fn real_http11_over_private_uds_enforces_auth_and_keeps_public_routes_absent() {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn exchange(path: &Path, request: String) -> String {
        let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    let token = [33_u8; 32];
    let state = protected_state(token);
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = temp.path().canonicalize().unwrap().join("admin.sock");
    let prepared =
        crate::admin_socket::prepare_admin_socket(&path, unsafe { libc::geteuid() }).unwrap();
    let (listener, cleanup) = prepared.into_parts();
    let app = crate::admin::admin_router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let bearer = URL_SAFE_NO_PAD.encode(token);
    let body = serde_json::json!({
        "kind_id": "opaque",
        "subject_id": 1,
        "enabled": true,
        "config_b64": "",
        "subject_grant": issue_test_subject_grant(&state),
    })
    .to_string();
    let protected = exchange(
        &path,
        format!(
            "PUT /api/gate-instances/00000000-0000-0000-0000-000000000000 HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {bearer}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(
        protected.starts_with("HTTP/1.1 201 Created\r\n"),
        "{protected}"
    );

    let absent = exchange(
        &path,
        "GET /api/public HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_owned(),
    )
    .await;
    assert!(absent.starts_with("HTTP/1.1 404 Not Found\r\n"), "{absent}");
    server.abort();
    drop(cleanup);
}
