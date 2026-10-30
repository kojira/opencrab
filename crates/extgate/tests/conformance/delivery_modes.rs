async fn hello_ok_operation_driven(s: &mut UnixStream, instance_id: &str) {
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
            "agent_id": "agent-1",
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
    let binding_id = "chan-1".to_string();
    let mut s = h.connect().await;
    hello_ok_operation_driven(&mut s, &instance_id).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "session": binding_id,
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
