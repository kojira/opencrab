#[tokio::test]
async fn dynamic_binding_put_keeps_old_said_and_new_not_ready() {
    let h = Harness::start().await;
    let (mut s, instance_id, binding_a) = ready_pair(&h).await;
    let binding_b = uuid();
    put_binding(&h, &binding_b, &instance_id, "chan-2").await;
    let _ = read_frame_opt(&mut s).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_a,
            "origin": "oa",
            "author_id": "u1",
            "text": "old",
            "attachments": []
        }),
    )
    .await;
    let ok = read_said_response(&mut s, "s1").await;
    assert_eq!(ok["m"], "ok");
    assert!(ok["seq"].as_i64().unwrap() >= 1);
    write_frame(
        &mut s,
        &json!({
            "id": "s2",
            "m": "said",
            "binding_id": binding_b,
            "origin": "ob",
            "author_id": "u1",
            "text": "new",
            "attachments": []
        }),
    )
    .await;
    let err = read_said_response(&mut s, "s2").await;
    assert_eq!(err["code"], "instance_not_ready");
}

#[tokio::test]
async fn live_revision_and_delete_are_409() {
    let h = Harness::start().await;
    let (_s, instance_id, _) = ready_pair(&h).await;
    let (st, body) = h
        .admin(
            Request::builder()
                .method("POST")
                .uri(format!("/api/gate-instances/{instance_id}/revisions"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "expected_revision": 1,
                        "enabled": true,
                        "config_b64": config_b64()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "instance_active");

    let (st, body) = h
        .admin(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/gate-instances/{instance_id}"))
                .header(header::AUTHORIZATION, auth())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "instance_active");
}

#[tokio::test]
async fn live_binding_delete_stops_said() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    let (st, _) = h
        .admin(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/gate-bindings/{binding_id}"))
                .header(header::AUTHORIZATION, auth())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(20)).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o",
            "author_id": "u",
            "text": "x",
            "attachments": []
        }),
    )
    .await;
    let v = read_said_response(&mut s, "s1").await;
    assert_eq!(v["code"], "binding_closed");
}

async fn grandfathered_harness(instance_id: &str) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("grandfathered.sqlite");
    {
        let conn = opencrab_db::init_connection(database.to_str().unwrap()).unwrap();
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS subject_allocator_no_delete;
             DROP TRIGGER IF EXISTS subject_allocator_monotonic;
             DROP TRIGGER IF EXISTS subject_tombstones_no_update;
             DROP TRIGGER IF EXISTS subject_tombstones_no_delete;
             DROP TRIGGER IF EXISTS subject_grants_no_delete;
             DROP TRIGGER IF EXISTS subject_grants_consume_once;
             DROP TRIGGER IF EXISTS agents_subject_id_insert_guard;
             DROP TRIGGER IF EXISTS agents_subject_id_assign;
             DROP TRIGGER IF EXISTS agents_subject_id_advance_explicit;
             DROP TRIGGER IF EXISTS agents_subject_id_update_guard;
             DROP TRIGGER IF EXISTS agents_subject_tombstone_delete_guard;
             DROP TABLE subject_association_grants;
             DROP TABLE subject_tombstones;
             DROP TABLE subject_id_allocator;
             ALTER TABLE gate_bindings DROP COLUMN session_id;
             ALTER TABLE gate_instances DROP COLUMN association_grandfathered;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (agent_id, name, persona_name, subject_id)
             VALUES ('agent-1', 'A', 'p', 41)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gate_instances
                 (instance_id, kind_id, subject_id, revision, enabled, config_b64,
                  config_digest, created_at, updated_at)
             VALUES (?1, 'opaque-kind', 41, 1, 1, ?2, ?3, 11, 11)",
            rusqlite::params![instance_id, config_b64(), config_digest()],
        )
        .unwrap();
        conn.execute_batch("PRAGMA user_version=53;").unwrap();
    }
    let db = opencrab_db::Db::open(database.to_str().unwrap()).unwrap();
    Harness::start_with_db(dir, db, 41).await
}

fn association_row_bytes(state: &ExtgateState, instance_id: &str) -> Vec<u8> {
    let conn = state.db.lock().unwrap();
    let row = conn
        .query_row(
            "SELECT instance_id, kind_id, subject_id, revision, enabled, config_b64,
                    config_digest, created_at, updated_at, deleted_at,
                    association_grandfathered
             FROM gate_instances WHERE instance_id=?1",
            [instance_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .unwrap();
    serde_json::to_vec(&row).unwrap()
}

fn grant_row_bytes(state: &ExtgateState) -> Vec<Vec<u8>> {
    let conn = state.db.lock().unwrap();
    let mut statement = conn
        .prepare(
            "SELECT grant_hash, agent_id, subject_id, expires_at, consumed_at, consumed_instance_id
             FROM subject_association_grants ORDER BY grant_hash",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok(serde_json::to_vec(&(
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
            .unwrap())
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows
}

#[tokio::test]
async fn s2_grandfathered_association_exact_put_needs_no_grant_and_changes_no_bytes() {
    let grandfathered = "00000000-0000-4000-8000-000000000041";
    let genuinely_new = "00000000-0000-4000-8000-000000000042";
    let h = grandfathered_harness(grandfathered).await;
    let before = association_row_bytes(&h.state, grandfathered);
    let grants_before = grant_row_bytes(&h.state);
    let request = |instance_id: &str| {
        Request::builder()
            .method("PUT")
            .uri(format!("/api/gate-instances/{instance_id}"))
            .header(header::AUTHORIZATION, auth())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "kind_id": "opaque-kind",
                    "subject_id": h.subject_id,
                    "enabled": true,
                    "config_b64": config_b64()
                })
                .to_string(),
            ))
            .unwrap()
    };

    let (status, _) = h.admin(request(grandfathered)).await;
    assert_eq!(status, StatusCode::OK, "grandfathered exact PUT required a grant");
    let (status, body) = h.admin(request(genuinely_new)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "instance_conflict");

    assert_eq!(association_row_bytes(&h.state, grandfathered), before);
    assert_eq!(
        grant_row_bytes(&h.state),
        grants_before,
        "grandfathered retry changed grant rows"
    );
    assert_eq!(
        h.state
            .db
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM gate_instances", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1,
        "genuinely new grantless association was persisted"
    );
}

#[tokio::test]
async fn s2_new_first_instance_association_without_grant_is_forbidden() {
    let h = Harness::start().await;
    let id = uuid();
    let (status, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "opaque-kind",
                        "subject_id": h.subject_id,
                        "enabled": true,
                        "config_b64": config_b64()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{status} {}", String::from_utf8_lossy(&body));
    assert_eq!(err_code(&body), "instance_conflict");
    let associations: i64 = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row("SELECT count(*) FROM gate_instances", [], |row| row.get(0))
        .unwrap();
    assert_eq!(associations, 0, "unauthorized first association was persisted");
}

#[tokio::test]
async fn s2_subject_grant_is_consumed_once_and_exact_retry_needs_no_second_grant() {
    let h = Harness::start().await;
    let first = uuid();
    let second = uuid();
    let grant = {
        let mut conn = h.state.db.lock().unwrap();
        opencrab_db::queries::issue_subject_association_grant(
            &mut conn,
            "agent-1",
            h.subject_id,
            i64::MAX,
            now_nanos(),
        )
        .unwrap()
    };
    let request = |instance_id: &str, grant: Option<&str>| {
        let mut body = json!({
            "kind_id": "opaque-kind",
            "subject_id": h.subject_id,
            "enabled": true,
            "config_b64": config_b64()
        });
        if let Some(grant) = grant {
            body["subject_grant"] = json!(grant);
        }
        Request::builder()
            .method("PUT")
            .uri(format!("/api/gate-instances/{instance_id}"))
            .header(header::AUTHORIZATION, auth())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let (status, _) = h.admin(request(&first, Some(&grant))).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = h.admin(request(&first, None)).await;
    assert_eq!(status, StatusCode::OK, "exact retry consumed another grant");
    let (status, body) = h.admin(request(&second, Some(&grant))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "instance_conflict");
    let consumed_instance: String = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT consumed_instance_id FROM subject_association_grants",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(consumed_instance, first);
}

#[tokio::test]
async fn instance_put_idempotent_and_conflict() {
    let h = Harness::start().await;
    let id = uuid();
    put_instance(&h, &id, true).await;
    let st = {
        let (st, _) = h
            .admin(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/api/gate-instances/{id}"))
                    .header(header::AUTHORIZATION, auth())
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "kind_id": "discord",
                            "subject_id": h.subject_id,
                            "enabled": true,
                            "config_b64": config_b64()
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
        st
    };
    assert_eq!(st, StatusCode::OK);
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "other",
                        "subject_id": h.subject_id,
                        "enabled": true,
                        "config_b64": config_b64()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "instance_conflict");
}

#[tokio::test]
async fn database_backed_bearer_rejections_are_exact_401() {
    let h = Harness::start().await;
    let id = uuid();
    let cases: Vec<Request<Body>> = vec![
        Request::builder()
            .method("GET")
            .uri(format!("/api/gate-instances/{id}"))
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("GET")
            .uri(format!("/api/gate-instances/{id}"))
            .header(header::AUTHORIZATION, "Basic x")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("GET")
            .uri(format!("/api/gate-instances/{id}"))
            .header(header::AUTHORIZATION, "Bearer ")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("GET")
            .uri(format!("/api/gate-instances/{id}"))
            .header(header::AUTHORIZATION, "Bearer short")
            .body(Body::empty())
            .unwrap(),
        Request::builder()
            .method("GET")
            .uri(format!("/api/gate-instances/{id}"))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}x"))
            .body(Body::empty())
            .unwrap(),
    ];
    for req in cases {
        let (st, body) = h.admin(req).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
        assert_eq!(body, UNAUTHORIZED_BODY);
    }
}

#[tokio::test]
async fn bearer_equal_reaches_operation() {
    let h = Harness::start().await;
    let id = uuid();
    let (st, body) = h
        .admin(
            Request::builder()
                .method("GET")
                .uri(format!("/api/gate-instances/{id}"))
                .header(header::AUTHORIZATION, auth())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(err_code(&body), "instance_unknown");
}

#[tokio::test]
async fn listen_socket_rejects_relative_and_nonsocket() {
    assert_eq!(validate_listen_socket("").unwrap(), None);
    assert!(validate_listen_socket("relative/path.sock").is_err());
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-socket");
    std::fs::write(&file, b"x").unwrap();
    assert!(validate_listen_socket(file.to_str().unwrap()).is_err());
}

#[tokio::test]
async fn lookups_unknown_address_is_false() {
    let h = Harness::start().await;
    let conn = h.state.db.lock().unwrap();
    assert!(!opencrab_extgate::channel_whitelisted(
        &conn, "agent-1", "missing", "nope"
    ));
    let _ = TRUSTED_PLATFORM_EXTGATE;
    let _ = session_id_for_binding("x");
}

#[tokio::test]
async fn closed_instance_put_does_not_revive() {
    let h = Harness::start().await;
    let id = uuid();
    put_instance(&h, &id, true).await;
    let (st, _) = h
        .admin(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/gate-instances/{id}"))
                .header(header::AUTHORIZATION, auth())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "discord",
                        "subject_id": h.subject_id,
                        "enabled": true,
                        "config_b64": config_b64()
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "instance_conflict");
}

#[tokio::test]
async fn address_in_use_and_binding_closed_reuse() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let a = uuid();
    let b = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &a, &instance_id, "same").await;
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-bindings/{b}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "instance_id": instance_id,
                        "address": "same",
                        "session": {"session_id": session_id_for_binding(&b), "title": "same"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "address_in_use");
    let (st, _) = h
        .admin(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/gate-bindings/{a}"))
                .header(header::AUTHORIZATION, auth())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-bindings/{a}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "instance_id": instance_id,
                        "address": "same",
                        "session": {"session_id": session_id_for_binding(&a), "title": "same"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "binding_closed");
}

