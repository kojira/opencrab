// Issue #1006 S3 assertion-level conformance tests.

async fn s3_hello_with_capabilities(
    stream: &mut UnixStream,
    instance_id: &str,
    config_digest: &str,
    operations: &Value,
    final_delivery: &str,
    delivery_guarantee: &str,
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
            "delivery_guarantee": delivery_guarantee,
            "operations": operations,
        }),
    )
    .await;
    read_frame(stream).await
}

#[tokio::test]
async fn s3_hello_final_delivery_rejects_both_legacy_config_mismatch_directions() {
    let h = Harness::start().await;

    let automatic_instance = uuid();
    put_instance(&h, &automatic_instance, true).await;
    let mut automatic_stream = h.connect().await;
    let operation_driven = s3_hello_with_capabilities(
        &mut automatic_stream,
        &automatic_instance,
        &config_digest(),
        &ops_reply(),
        "operation_driven",
        "at_most_once_indeterminate",
    )
    .await;
    assert_eq!(operation_driven["m"], "err");
    assert_eq!(operation_driven["code"], "operation_declaration_invalid");

    let driven_instance = uuid();
    put_instance(&h, &driven_instance, true).await;
    let driven_config = br#"{"delivery_mode":"tool_driven"}"#;
    let driven_b64 = base64::engine::general_purpose::STANDARD.encode(driven_config);
    let driven_digest = opencrab_extgate::ids::config_digest_from_b64(&driven_b64).unwrap();
    h.state
        .db
        .lock()
        .unwrap()
        .execute(
            "UPDATE gate_instances SET config_b64 = ?1, config_digest = ?2 WHERE instance_id = ?3",
            rusqlite::params![driven_b64, driven_digest, driven_instance],
        )
        .unwrap();
    let mut driven_stream = h.connect().await;
    let automatic = s3_hello_with_capabilities(
        &mut driven_stream,
        &driven_instance,
        &driven_digest,
        &ops_reply(),
        "automatic",
        "at_most_once_indeterminate",
    )
    .await;
    assert_eq!(automatic["m"], "err");
    assert_eq!(automatic["code"], "operation_declaration_invalid");
}

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

#[tokio::test]
async fn s3_exact_runtime_rejects_explicit_weaker_invocation_before_db_and_wire() {
    let h = Harness::start().await;
    let instance_id = uuid();
    let binding_id = uuid();
    put_instance(&h, &instance_id, true).await;
    put_binding(&h, &binding_id, &instance_id, "s3-no-downgrade").await;
    let mut stream = h.connect().await;
    assert_eq!(
        s3_hello_with_capabilities(
            &mut stream,
            &instance_id,
            &config_digest(),
            &ops_reply(),
            "automatic",
            "exactly_once",
        )
        .await["m"],
        "ok"
    );
    assert_eq!(ack_bind(&mut stream).await, binding_id);
    wait_acked(&h, &instance_id, &binding_id).await;

    let out = invoke_and_wait_with_requirement(
        &h.state,
        &instance_id,
        &binding_id,
        "reply",
        Some(DeliveryGuarantee::AtMostOnceIndeterminate),
        &json!({"event": "e1", "text": "must not downgrade"}),
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
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "guarantee downgrade wrote a call row");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), read_frame(&mut stream))
            .await
            .is_err(),
        "guarantee downgrade wrote an invoke frame"
    );
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
