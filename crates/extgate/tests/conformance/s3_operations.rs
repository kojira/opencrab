// Issue #1006 S3 assertion-level conformance tests.

async fn s3_hello_with_capabilities(
    stream: &mut UnixStream,
    instance_id: &str,
    config_digest: &str,
    operations: &Value,
    final_delivery: &str,
) -> Value {
    write_frame(
        stream,
        &json!({
            "id": "s3-hello",
            "m": "hello",
            "protocol": 3,
            "operation_protocol": 1,
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": config_digest,
            "final_delivery": final_delivery,
            "operations": operations,
        }),
    )
    .await;
    read_frame(stream).await
}

#[tokio::test]
async fn s3_hello_final_delivery_is_not_config_authority() {
    let h = Harness::start().await;

    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let mut stream = h.connect().await;
    let operation_driven = s3_hello_with_capabilities(
        &mut stream,
        &instance_id,
        &config_digest(),
        &ops_reply(),
        "operation_driven",
    )
    .await;
    assert_eq!(operation_driven["m"], "ok");
    assert_eq!(operation_driven["id"], "s3-hello");
}

#[tokio::test]
async fn s3_projection_rejects_stale_live_declaration_digest_before_db_and_wire() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "s3-stale-projection").await;
    let declaration = |caller: &str| {
        json!([{
            "name": "quasar.synthetic-v7",
            "description": "synthetic",
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"},
            "callback_schema": null,
            "authorization": {"allowed_callers": [caller]},
            "dispatch": "background",
            "sub_engine": "not_exposed",
            "sharing": "agent_bound",
            "effect": "state_change"
        }])
    };

    let mut first = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut first, &instance_id, 1, &declaration("owner")).await["m"],
        "ok"
    );
    assert_eq!(ack_bind(&mut first).await, binding_id);
    wait_acked(&h, &instance_id, &binding_id).await;
    let projected = ExtgateOpsGatewayActions::for_binding(
        Arc::clone(&h.state),
        &instance_id,
        &binding_id,
        &session_id_for_binding(&binding_id),
        "agent-1",
    )
    .unwrap();
    drop(first);
    wait_not_live(&h, &instance_id).await;

    let mut second = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut second, &instance_id, 1, &declaration("guest")).await["m"],
        "ok"
    );
    assert_eq!(ack_bind(&mut second).await, binding_id);
    wait_acked(&h, &instance_id, &binding_id).await;

    let result = tokio::time::timeout(
        Duration::from_secs(1),
        projected.execute(
            "quasar.synthetic-v7",
            &json!({}),
            &GatewayCallContext::new(GatewayCaller::Owner, "agent-1"),
        ),
    )
    .await
    .expect("stale projection must fail without waiting for a gateway response");
    assert_eq!(result.error.as_deref(), Some("operation_rejected"));
    let count: i64 = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM gateway_operation_calls WHERE binding_id = ?1",
            [&binding_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "stale projection wrote a call row");
    assert!(read_frame_opt(&mut second).await.is_none(), "stale projection wrote a frame");
}

/// S3: an arbitrary synthetic operation name is projected and authorized solely from declared
/// metadata. A denied caller produces neither a call row nor wire traffic; an allowed caller uses
/// the same name without any shared allowlist or name classification.
#[tokio::test]
async fn s3_arbitrary_synthetic_operation_projects_and_authorizes_from_metadata() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "s3-synthetic").await;
    let declarations = json!([{
        "name": "quasar.synthetic-v7",
        "description": "synthetic metadata-only operation",
        "input_schema": {"type": "object"},
        "output_schema": {"type": "object"},
        "callback_schema": null,
        "authorization": {"allowed_callers": ["owner"]},
        "dispatch": "background",
        "sub_engine": "not_exposed",
        "sharing": "agent_bound",
        "effect": "state_change"
    }]);
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &declarations).await["m"],
        "ok"
    );
    assert_eq!(ack_bind(&mut s).await, binding_id);
    wait_acked(&h, &instance_id, &binding_id).await;

    let session_id = session_id_for_binding(&binding_id);
    let projected = ExtgateOpsGatewayActions::for_binding(
        Arc::clone(&h.state),
        &instance_id,
        &binding_id,
        &session_id,
        "agent-1",
    )
    .unwrap();
    assert!(projected
        .definitions()
        .iter()
        .any(|definition| definition.name == "quasar.synthetic-v7"));

    let denied = projected
        .execute(
            "quasar.synthetic-v7",
            &json!({"probe": "denied"}),
            &GatewayCallContext::for_agent("agent-1"),
        )
        .await;
    assert_eq!(denied.error.as_deref(), Some("operation_unauthorized"));
    let count: i64 = h
        .state
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM gateway_operation_calls WHERE binding_id = ?1",
            [&binding_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "denied caller wrote a call row");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), read_frame(&mut s))
            .await
            .is_err(),
        "denied caller wrote an invoke frame"
    );

    let allowed = tokio::spawn(async move {
        projected
            .execute(
                "quasar.synthetic-v7",
                &json!({"probe": "allowed"}),
                &GatewayCallContext::new(GatewayCaller::Owner, "agent-1"),
            )
            .await
    });
    let invoke = read_until(&mut s, |frame| frame["m"] == "invoke").await;
    assert_eq!(invoke["operation"], "quasar.synthetic-v7");
    assert_eq!(invoke["payload"]["probe"], "allowed");
    let call_id = invoke["id"].as_str().unwrap();
    write_frame(
        &mut s,
        &json!({"id": call_id, "m": "ok", "result": {"accepted": true}}),
    )
    .await;
    let result = allowed.await.unwrap();
    assert!(result.success, "allowed owner was rejected: {:?}", result.error);
    assert_eq!(result.data, Some(json!({"accepted": true})));
}
