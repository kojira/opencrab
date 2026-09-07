#[test]
fn disconnected_create_is_202_then_ready_after_hello() {
    let mock = spawn_mock_llm();
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("e2e.db");
    let sock = PathBuf::from(format!("/tmp/wg-disc-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let core_port = free_port();
    let gw_port = free_port();
    let core = seed_core(root.path(), &db, &sock, core_port, mock.port);

    let (st, body) = http(
        core_port,
        "POST",
        &format!("/api/agents/{AGENT}/web-conversations"),
        None,
        Some("{}"),
        Duration::from_secs(10),
    )
    .expect("create");
    assert_eq!(st, 202, "{body}");
    let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(v["state"], "provisioning");
    let session = v["session_id"].as_str().unwrap().to_string();
    assert_eq!(counts(&db), (1, 1, 1));

    let (st, detail) = http(
        core_port,
        "GET",
        &format!("/api/sessions/{session}"),
        None,
        None,
        Duration::from_secs(5),
    )
    .expect("detail");
    let d: serde_json::Value = serde_json::from_str(detail.trim()).unwrap();
    assert_eq!(st, 200);
    assert_eq!(d["web_binding_state"], "unavailable");

    let gw = spawn_gateway(root.path(), &sock, gw_port);
    assert!(wait_tcp(gw_port, Duration::from_secs(15)), "gateway http");

    let start = Instant::now();
    let mut ready = None;
    while start.elapsed() < Duration::from_secs(15) {
        if let Some((st, detail)) = http(
            core_port,
            "GET",
            &format!("/api/sessions/{session}"),
            None,
            None,
            Duration::from_secs(2),
        ) {
            if st == 200 {
                let d: serde_json::Value = serde_json::from_str(detail.trim()).unwrap();
                if d["web_binding_state"] == "ready" {
                    ready = Some(d);
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ready.is_some(), "detail never became ready");
    assert_eq!(
        counts(&db),
        (1, 1, 1),
        "must not duplicate the conversation"
    );

    mock.release();
    let post_body = format!(
        r#"{{"client_message_id":"{CLIENT_MSG}","text":"hello from e2e","attachments":[]}}"#
    );
    let (st, accepted) = http(
        gw_port,
        "POST",
        &format!("/api/web-conversations/{session}/messages"),
        None,
        Some(&post_body),
        Duration::from_secs(10),
    )
    .expect("message");
    assert_eq!(st, 202, "{accepted}");
    let a: serde_json::Value = serde_json::from_str(accepted.trim()).unwrap();
    assert_ne!(a["error"]["code"], "instance_not_ready", "{accepted}");
    assert_eq!(a["state"], "accepted", "{accepted}");
    drop(gw);
    drop(core);
    let _ = std::fs::remove_file(&sock);
}
