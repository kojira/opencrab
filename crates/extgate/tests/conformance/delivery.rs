#[tokio::test]
async fn delivery_ok_rejected_and_disconnect() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "d1",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    let mut saw_say = None;
    for _ in 0..50 {
        if let Some(v) = read_frame_opt(&mut s).await {
            if v["m"] == "say" {
                saw_say = Some(v);
                break;
            }
        }
    }
    let say = saw_say.expect("say");
    // 単一メンション turn の say は発端 said の origin を reply_target に載せる（gateway が
    // e-tag reply する。裁定A で ended は say の後になったが、返信先は payload で明示する方針）。
    assert_eq!(
        say["payload"],
        json!({"text": "hello from agent", "reply_target": "d1"})
    );
    assert!(!say["payload"]["text"].as_str().unwrap().is_empty());
    write_frame(&mut s, &json!({"id": say["id"], "m": "ok"})).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let conn = h.state.db.lock().unwrap();
    let state: String = conn
        .query_row(
            "SELECT state FROM deliveries WHERE delivery_id = ?1",
            [say["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "delivered");
    drop(conn);

    *h.runtime.reply.lock().unwrap() = "second".into();
    write_frame(
        &mut s,
        &json!({
            "id": "s2",
            "m": "said",
            "binding_id": binding_id,
            "origin": "d2",
            "author_id": "u1",
            "text": "again",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    let mut say2 = None;
    for _ in 0..50 {
        if let Some(v) = read_frame_opt(&mut s).await {
            if v["m"] == "say" {
                say2 = Some(v);
                break;
            }
        }
    }
    let say2 = say2.expect("say2");
    write_frame(
        &mut s,
        &json!({
            "id": say2["id"],
            "m": "err",
            "code": "external_rejected",
            "detail": null
        }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let conn = h.state.db.lock().unwrap();
    let (st, err): (String, String) = conn
        .query_row(
            "SELECT state, error FROM deliveries WHERE delivery_id = ?1",
            [say2["id"].as_str().unwrap()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(st, "failed");
    assert_eq!(err, "external_rejected");
}

#[tokio::test]
async fn delivery_disconnect_is_indeterminate_and_no_resend() {
    let h = Harness::start().await;
    let (mut s, instance_id, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "cut",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    let mut delivery_id = None;
    for _ in 0..50 {
        if let Some(v) = read_frame_opt(&mut s).await {
            if v["m"] == "say" {
                delivery_id = Some(v["id"].as_str().unwrap().to_string());
                break;
            }
        }
    }
    let delivery_id = delivery_id.expect("say");
    drop(s);
    tokio::time::sleep(Duration::from_millis(80)).await;
    let conn = h.state.db.lock().unwrap();
    let (st, err): (String, String) = conn
        .query_row(
            "SELECT state, error FROM deliveries WHERE delivery_id = ?1",
            [&delivery_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(st, "indeterminate");
    assert_eq!(err, "disconnect");
    drop(conn);

    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let _ = ack_bind(&mut s).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    if let Some(v) = read_frame_opt(&mut s).await {
        assert_ne!(v["id"], delivery_id);
    }
}

#[tokio::test]
async fn delivery_failure_injection_rolls_back_and_says_zero() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    h.state.probe.fail_reply_log.store(true, Ordering::SeqCst);
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "inj",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame(&mut s).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let conn = h.state.db.lock().unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn startup_recover_stale_sending() {
    let db = opencrab_db::Db::memory().unwrap();
    {
        let conn = db.lock().unwrap();
        opencrab_db::queries::upsert_agent(
            &conn,
            &AgentRow {
                agent_id: "agent-1".into(),
                name: "A".into(),
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
        let sid: i64 = conn
            .query_row(
                "SELECT subject_id FROM agents WHERE agent_id='agent-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO gate_instances (
                instance_id, kind_id, subject_id, revision, enabled,
                config_b64, config_digest, created_at, updated_at
             ) VALUES ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa','k',?1,1,1,'e30=','bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',1,1)",
            [sid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gate_bindings (binding_id, instance_id, address, created_at)
             VALUES ('bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb','aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa','a',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO deliveries (delivery_id, binding_id, payload_json, state, error, created_at, updated_at)
             VALUES ('cccccccc-cccc-cccc-cccc-cccccccccccc','bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb','{\"text\":\"x\"}','sending',NULL,1,1)",
            [],
        )
        .unwrap();
    }
    {
        let mut conn = db.lock().unwrap();
        recover_stale_deliveries(&mut conn, 99).unwrap();
        let (st, err): (String, String) = conn
            .query_row(
                "SELECT state, error FROM deliveries WHERE delivery_id='cccccccc-cccc-cccc-cccc-cccccccccccc'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(st, "indeterminate");
        assert_eq!(err, "stale sending recovered after restart");
    }
}

