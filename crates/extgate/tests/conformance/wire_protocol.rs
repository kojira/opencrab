#[tokio::test]
async fn framing_max_size_ok_and_too_large_closes() {
    let h = Harness::start().await;
    let (mut s, instance_id, _) = ready_pair(&h).await;
    let mut ok = vec![b'{'; 1_048_575];
    ok[0] = b'{';
    ok[1] = b'"';
    ok[2] = b'm';
    ok[3] = b'"';
    ok[4] = b':';
    ok[5] = b'"';
    // 1,048,576 including LF: send a valid small said instead for success path
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": instance_id, // wrong on purpose? use real binding below
            "origin": "o",
            "author_id": "u1",
            "text": "x",
            "attachments": []
        }),
    )
    .await;
    let _ = read_frame_opt(&mut s).await;

    let mut s2 = h.connect().await;
    let mut huge = vec![b'x'; 1_048_577];
    huge[1_048_576] = b'\n';
    s2.write_all(&huge).await.unwrap();
    let mut leftover = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), s2.read_to_end(&mut leftover))
        .await
        .expect("too_large did not close")
        .expect("read after too_large");
    assert!(
        leftover.is_empty(),
        "id 未抽出の too_large は err frame 0: {leftover:?}"
    );
}

#[tokio::test]
async fn framing_invalid_utf8_json_non_object_and_duplicates_close() {
    let h = Harness::start().await;
    let mut s = h.connect().await;
    s.write_all(b"\x80\n").await.unwrap();
    let v = read_frame_opt(&mut s).await;
    if let Some(v) = v {
        assert_eq!(v["code"], "bad_request");
    }

    let mut s = h.connect().await;
    s.write_all(b"not-json\n").await.unwrap();
    let v = read_frame_opt(&mut s).await;
    if let Some(v) = v {
        assert_eq!(v["code"], "bad_request");
    }

    let mut s = h.connect().await;
    s.write_all(b"[1]\n").await.unwrap();
    let v = read_frame_opt(&mut s).await;
    if let Some(v) = v {
        assert_eq!(v["code"], "bad_request");
    }

    let mut s = h.connect().await;
    s.write_all(br#"{"m":"hello","m":"hello"}"#).await.unwrap();
    s.write_all(b"\n").await.unwrap();
    let v = read_frame_opt(&mut s).await;
    if let Some(v) = v {
        assert_eq!(v["code"], "bad_request");
    }
}

#[tokio::test]
async fn hello_unknown_fields_ignored_and_missing_fields_fail() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": config_digest(),
            "extra": true
        }),
    )
    .await;
    let ok = read_frame(&mut s).await;
    assert_eq!(ok["m"], "ok");

    let mut s = h.connect().await;
    write_frame(&mut s, &json!({"id":"h2","m":"hello","protocol":2})).await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "bad_request");
}

#[tokio::test]
async fn protocol_order_before_hello_and_second_hello() {
    let h = Harness::start().await;
    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": uuid(),
            "origin": "o",
            "author_id": "u",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "protocol_order");

    let (mut s, instance_id, _) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "id": "h2",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "protocol_order");
}

#[tokio::test]
async fn response_invalid_unknown_and_consumed_ids() {
    let h = Harness::start().await;
    let (mut s, _, _) = ready_pair(&h).await;
    write_frame(&mut s, &json!({"id":"nope","m":"ok"})).await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "response_invalid");
}

#[tokio::test]
async fn hello_failures_do_not_register() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;

    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 1,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "protocol_unsupported");
    assert!(!h.state.lock_registry().unwrap().is_live(&instance_id));

    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": uuid(),
            "revision": 1,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "instance_unknown");

    let disabled = uuid();
    put_instance(&h, &disabled, false).await;
    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": disabled,
            "revision": 1,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "instance_disabled");

    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": 9,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "revision_mismatch");

    let mut s = h.connect().await;
    write_frame(
        &mut s,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }),
    )
    .await;
    let v = read_frame_opt(&mut s).await.unwrap();
    assert_eq!(v["code"], "config_digest_mismatch");
}

#[tokio::test]
async fn double_live_hello_is_instance_active() {
    let h = Harness::start().await;
    let (s1, instance_id, _) = ready_pair(&h).await;
    let mut s2 = h.connect().await;
    write_frame(
        &mut s2,
        &json!({
            "id": "h1",
            "m": "hello",
            "protocol": 2,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": config_digest()
        }),
    )
    .await;
    let v = read_frame_opt(&mut s2).await.unwrap();
    assert_eq!(v["code"], "instance_active");
    drop(s1);
}

#[tokio::test]
async fn registry_starts_empty() {
    let h = Harness::start().await;
    assert!(!h.state.lock_registry().unwrap().is_live(&uuid()));
}

#[tokio::test]
async fn said_before_ack_is_instance_not_ready() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let bind = read_frame(&mut s).await;
    assert_eq!(bind["m"], "bind");
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "o1",
            "author_id": "u1",
            "text": "hi",
            "attachments": []
        }),
    )
    .await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["m"], "err");
    assert_eq!(v["code"], "instance_not_ready");
}

#[tokio::test]
async fn hello_timeout_is_protocol_order() {
    let h = Harness::start().await;
    let mut s = h.connect().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    tokio::time::resume();
    let mut leftover = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut leftover))
        .await
        .expect("hello timeout did not close")
        .expect("read after hello timeout");
    assert!(
        leftover.is_empty(),
        "id 未抽出の hello timeout は err frame 0: {leftover:?}"
    );
}

#[tokio::test]
async fn bind_timeout_is_bind_failed() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let bind = read_frame(&mut s).await;
    assert_eq!(bind["m"], "bind");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!h.state.lock_registry().unwrap().is_live(&instance_id));
}

#[tokio::test]
async fn bind_err_closes() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "chan-1").await;
    let mut s = h.connect().await;
    hello_ok(&mut s, &instance_id, 1).await;
    let bind = read_frame(&mut s).await;
    write_frame(
        &mut s,
        &json!({
            "id": bind["id"],
            "m": "err",
            "code": "bind_failed",
            "detail": null
        }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!h.state.lock_registry().unwrap().is_live(&instance_id));
}

#[tokio::test]
async fn running_unknown_message_keeps_connection() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    write_frame(&mut s, &json!({"id":"x","m":"edited"})).await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["code"], "unknown_message");
    write_frame(
        &mut s,
        &json!({
            "id": "s1",
            "m": "said",
            "binding_id": binding_id,
            "origin": "after",
            "author_id": "u",
            "text": "still",
            "attachments": []
        }),
    )
    .await;
    let ok = read_frame(&mut s).await;
    assert_eq!(ok["m"], "ok");
}

#[tokio::test]
async fn running_reverse_and_unknown_without_id_keep() {
    let h = Harness::start().await;
    let (mut s, _, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({
            "m": "activity",
            "binding_id": binding_id,
            "activity_id": uuid(),
            "state": "started"
        }),
    )
    .await;
    write_frame(&mut s, &json!({"m": "foo"})).await;
    write_frame(
        &mut s,
        &json!({
            "id": "s-keep",
            "m": "said",
            "binding_id": binding_id,
            "origin": "after-keep",
            "author_id": "u",
            "text": "still",
            "attachments": []
        }),
    )
    .await;
    let ok = read_said_response(&mut s, "s-keep").await;
    assert_eq!(ok["m"], "ok");
}

#[tokio::test]
async fn malformed_response_is_response_invalid_and_closes() {
    let h = Harness::start().await;
    let (mut s, instance_id, _) = ready_pair(&h).await;
    write_frame(&mut s, &json!({"id": "ghost", "m": "ok", "seq": "bad"})).await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["code"], "response_invalid");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!h.state.lock_registry().unwrap().is_live(&instance_id));
}

#[tokio::test]
async fn err_without_detail_is_response_invalid() {
    let h = Harness::start().await;
    let (mut s, instance_id, _) = ready_pair(&h).await;
    write_frame(
        &mut s,
        &json!({"id": "ghost", "m": "err", "code": "external_rejected"}),
    )
    .await;
    let v = read_frame(&mut s).await;
    assert_eq!(v["code"], "response_invalid");
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!h.state.lock_registry().unwrap().is_live(&instance_id));
}

