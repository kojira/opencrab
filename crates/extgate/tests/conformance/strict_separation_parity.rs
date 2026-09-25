#[tokio::test]
async fn strict_separation_hello_has_no_delivery_guarantee_contract() {
    let h = Harness::start().await;
    let instance_id = uuid();
    put_instance(&h, &instance_id, true).await;
    let mut stream = h.connect().await;
    write_frame(
        &mut stream,
        &json!({
            "id": "strict-hello",
            "m": "hello",
            "protocol": 3,
            "operation_protocol": 1,
            "final_delivery": "automatic",
            "operations": [],
            "instance_id": instance_id,
            "revision": 1,
            "config_digest": config_digest(),
        }),
    )
    .await;
    let response = read_frame(&mut stream).await;
    assert_eq!(response["m"], "ok", "{response}");
    assert_eq!(response["id"], "strict-hello");
}

#[tokio::test]
async fn strict_separation_co_agent_admission_uses_the_historical_role_snapshot() {
    let h = Harness::start().await;
    let (mut stream, _, binding_id) = ready_pair(&h).await;
    write_frame(
        &mut stream,
        &json!({
            "id": "strict-co-agent",
            "m": "said",
            "binding_id": binding_id,
            "origin": "strict-origin",
            "author_id": "external-co-agent",
            "caller": {"role": "co_agent", "agent_id": "peer-agent"},
            "text": "historical snapshot",
            "attachments": []
        }),
    )
    .await;
    let response = read_said_response(&mut stream, "strict-co-agent").await;
    assert_eq!(response["m"], "ok", "{response}");
}
