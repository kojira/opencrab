#[tokio::test]
async fn binding_put_reuses_existing_session_and_said_writes_there() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    insert_named_session(&h, "nostr-agent-1");
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "nostr-agent-1").await;
    {
        let conn = h.state.db.lock().unwrap();
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
        assert!(opencrab_db::queries::get_session(&conn, "nostr-agent-1")
            .unwrap()
            .is_some());
        assert!(
            opencrab_db::queries::get_session(&conn, &session_id_for_binding(&binding_id))
                .unwrap()
                .is_none()
        );
    }
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let acked = ack_bind(&mut s).await;
    assert_eq!(acked, binding_id);
    for _ in 0..50 {
        let acked = h
            .state
            .lock_registry()
            .unwrap()
            .get(&instance_id)
            .is_some_and(|e| e.acknowledged.contains(&binding_id));
        if acked {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "reuse-1",
            "author_id": "u1",
            "text": "hello reuse",
            "attachments": []
        }),
    )
    .await;
    let v = read_said_response(&mut s, "s1").await;
    assert_eq!(v["seq"], 1);
    let conn = h.state.db.lock().unwrap();
    let session_id: String = conn
        .query_row(
            "SELECT session_id FROM memory_sessions WHERE content = 'hello reuse'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(session_id, "nostr-agent-1");
}

#[tokio::test]
async fn binding_put_reuse_membership_mismatch_conflicts() {
    let h = Harness::start().await;
    {
        let conn = h.state.db.lock().unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &AgentRow {
                agent_id: "agent-2".into(),
                name: "B".into(),
                job_title: None,
                organization: None,
                image_url: None,
                persona_name: "p".into(),
                personality: None,
                instructions: String::new(),
                heartbeat_instructions: String::new(),
                model: None,
                reasoning_effort: None,
                web_search: None,
                metadata_json: None,
            },
        )
        .unwrap();
        opencrab_db::queries::insert_session(
            &conn,
            &SessionRow {
                id: "owned-by-2".into(),
                mode: "solo".into(),
                theme: "x".into(),
                phase: "convergent".into(),
                turn_number: 0,
                status: "active".into(),
                participant_ids_json: r#"["agent-2"]"#.into(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            },
        )
        .unwrap();
    }
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-bindings/{binding_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"instance_id": instance_id, "address": "owned-by-2"}).to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "binding_conflict");
    let conn = h.state.db.lock().unwrap();
    let bindings: i64 = conn
        .query_row("SELECT COUNT(*) FROM gate_bindings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(bindings, 0);
}

