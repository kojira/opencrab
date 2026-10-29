const TOOL_DRIVEN_B64: &str = "eyJkZWxpdmVyeV9tb2RlIjoidG9vbF9kcml2ZW4ifQ==";

fn tool_driven_digest() -> String {
    opencrab_extgate::ids::config_digest_from_b64(TOOL_DRIVEN_B64).unwrap()
}

async fn put_instance_config(h: &Harness, instance_id: &str, config_b64: &str) -> Value {
    put_instance(h, instance_id, true).await;
    if config_b64 != self::config_b64() {
        // Re-declare the config as `provision` would, keeping revision 1 for the hello below.
        let conn = h.state.db.lock().unwrap();
        conn.execute(
            "UPDATE gate_instances SET config_b64=?2, config_digest=?3 WHERE instance_id=?1",
            rusqlite::params![
                instance_id,
                config_b64,
                opencrab_extgate::ids::config_digest_from_b64(config_b64).unwrap()
            ],
        )
        .unwrap();
    }
    json!({"instance_id": instance_id, "revision": 1})
}

async fn hello_ok_digest(s: &mut UnixStream, instance_id: &str, revision: u64, digest: &str) {
    write_frame(
        s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 3,
            "operation_protocol": 1,
            "final_delivery": "operation_driven",
            "operations": ops_reply(),
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
async fn operation_driven_inbound_plain_text_is_delivered_without_operation_call() {
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
    let mut saw_say = false;
    while let Some(frame) = read_frame_opt(&mut s).await {
        if frame["m"] == "say" {
            saw_say = true;
            assert_eq!(frame["payload"]["text"], "hello from agent");
            break;
        }
    }
    assert!(
        saw_say,
        "operation-capable Nostr-style bindings must still deliver plain text without an operation call"
    );
    let conn = h.state.db.lock().unwrap();
    let deliveries: i64 = conn
        .query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(deliveries, 1);
}
