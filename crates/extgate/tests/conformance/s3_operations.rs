// Issue #1006 S3 assertion-level conformance tests.

/// S3: an invocation may independently raise the delivery requirement. A live runtime that
/// only offers at-most-once-indeterminate must reject exactly-once before creating a call row or
/// writing an invoke frame.
#[tokio::test]
async fn s3_raised_exactly_once_is_rejected_before_db_and_wire() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "s3-guarantee").await;
    let mut s = h.connect().await;
    assert_eq!(
        hello_with_ops(&mut s, &instance_id, 1, &ops_reply()).await["m"],
        "ok"
    );
    assert_eq!(ack_bind(&mut s).await, binding_id);
    wait_acked(&h, &instance_id, &binding_id).await;

    let out = invoke_and_wait_with_requirement(
        &h.state,
        &instance_id,
        &binding_id,
        "reply",
        Some(DeliveryGuarantee::ExactlyOnce),
        &json!({"event": "e1", "text": "must not send"}),
    )
    .await;
    assert_eq!(out.unwrap_err().code.as_str(), "operation_rejected");

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
    assert_eq!(count, 0, "incompatible guarantee wrote a call row");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), read_frame(&mut s))
            .await
            .is_err(),
        "incompatible guarantee wrote an invoke frame"
    );
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
