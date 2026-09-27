#[tokio::test]
async fn provision_uses_canonical_binding_session_and_said_writes_there() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "nostr-agent-1").await;
    {
        let conn = h.state.db.lock().unwrap();
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
        assert!(
            opencrab_db::queries::get_session(&conn, &session_id_for_binding(&binding_id))
                .unwrap()
                .is_some()
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
    assert_eq!(session_id, session_id_for_binding(&binding_id));
}

