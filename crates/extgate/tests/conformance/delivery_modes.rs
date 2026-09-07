const TOOL_DRIVEN_B64: &str = "eyJkZWxpdmVyeV9tb2RlIjoidG9vbF9kcml2ZW4ifQ==";

fn tool_driven_digest() -> String {
    opencrab_extgate::ids::config_digest_from_b64(TOOL_DRIVEN_B64).unwrap()
}

async fn put_instance_config(h: &Harness, instance_id: &str, config_b64: &str) -> Value {
    let (st, body) = h
        .admin(
            Request::builder()
                .method("PUT")
                .uri(format!("/api/gate-instances/{instance_id}"))
                .header(header::AUTHORIZATION, auth())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "kind_id": "discord",
                        "subject_id": h.subject_id,
                        "enabled": true,
                        "config_b64": config_b64,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    assert!(
        st == StatusCode::CREATED || st == StatusCode::OK,
        "{st} {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}

async fn hello_ok_digest(s: &mut UnixStream, instance_id: &str, revision: u64, digest: &str) {
    write_frame(
        s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": revision,
            "config_digest": digest,
        }),
    )
    .await;
    let ok = read_frame(s).await;
    assert_eq!(ok["m"], "ok");
    assert_eq!(ok["id"], "h1");
}

#[tokio::test]
async fn tool_driven_inbound_is_no_reply_without_say() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance_config(&h, &instance_id, TOOL_DRIVEN_B64).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let mut s = h.connect().await;
    hello_ok_digest(&mut s, &instance_id, 1, &tool_driven_digest()).await;
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
            "origin": "td1",
            "author_id": "u1",
            "text": "ask",
            "attachments": []
        }),
    )
    .await;
    let v = read_said_response(&mut s, "s1").await;
    assert_eq!(v["seq"], 1);
    for _ in 0..50 {
        if h.runtime.turns.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.runtime.turns.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(80)).await;
    while let Some(frame) = read_frame_opt(&mut s).await {
        assert_ne!(frame["m"], "say", "{frame}");
    }
    let conn = h.state.db.lock().unwrap();
    let deliveries: i64 = conn
        .query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(deliveries, 0);
    // #899: 沈黙（NO_REPLY 終端）は speech として残さない。裸 NO_REPLY を永続すると
    // conversation_typed が `assistant: 'NO_REPLY'` としてモデルへ再注入する。
    let no_reply: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE content = 'NO_REPLY'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        no_reply, 0,
        "沈黙ターンで NO_REPLY 行を永続してはならない（#899）"
    );
}

#[tokio::test]
async fn missing_delivery_mode_keeps_say() {
    assert_eq!(
        opencrab_extgate::delivery_mode_from_config_bytes(b"{}").unwrap(),
        opencrab_extgate::DeliveryMode::Say
    );
    assert!(opencrab_extgate::dispatches_v3_say(
        opencrab_extgate::DeliveryMode::Say
    ));
    assert!(!opencrab_extgate::dispatches_v3_say(
        opencrab_extgate::DeliveryMode::ToolDriven
    ));
}
