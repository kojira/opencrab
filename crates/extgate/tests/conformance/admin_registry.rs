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
async fn bearer_exact_401_and_env_scrub() {
    std::env::set_var("OPENCRAB_GATE_OPERATOR_TOKEN", "env-secret");
    let token = OperatorToken::take_from_env();
    assert!(std::env::var("OPENCRAB_GATE_OPERATOR_TOKEN").is_err());
    assert!(format!("{token:?}").contains("redacted"));
    assert!(!format!("{token:?}").contains("env-secret"));

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
                    json!({"instance_id": instance_id, "address": "same"}).to_string(),
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
                    json!({"instance_id": instance_id, "address": "same"}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "binding_closed");
}

