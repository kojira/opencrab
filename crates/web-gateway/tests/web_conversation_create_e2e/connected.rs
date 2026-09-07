#[test]
fn connected_create_is_201_then_sse_said_turn_say() {
    let mock = spawn_mock_llm();
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("e2e.db");
    let sock = PathBuf::from(format!("/tmp/wg-create-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let core_port = free_port();
    let gw_port = free_port();
    let core = seed_core(root.path(), &db, &sock, core_port, mock.port);
    let gw = spawn_gateway(root.path(), &sock, gw_port);
    assert!(wait_tcp(gw_port, Duration::from_secs(15)), "gateway http");

    let (st, body) = http(
        core_port,
        "POST",
        &format!("/api/agents/{AGENT}/web-conversations"),
        None,
        Some(r#"{"name":"E2E"}"#),
        Duration::from_secs(70),
    )
    .expect("create");
    assert_eq!(st, 201, "{body}");
    let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(v["state"], "ready");
    assert_eq!(v["name"], "E2E");
    let session = v["session_id"].as_str().unwrap().to_string();
    let binding = v["binding_id"].as_str().unwrap().to_string();
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
    assert_eq!(st, 200, "{detail}");
    let d: serde_json::Value = serde_json::from_str(detail.trim()).unwrap();
    assert_eq!(d["web_binding_state"], "ready");

    let sse_rx = spawn_sse(gw_port, &session);
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
    assert_eq!(a["state"], "accepted");
    mock.release();
    let sse = sse_rx.recv_timeout(Duration::from_secs(30)).expect("sse");
    assert!(
        sse.contains("event: message") && sse.contains(REPLY),
        "{sse}"
    );

    let conn = Connection::open(&db).unwrap();
    let inbound: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1 AND content = 'hello from e2e'",
            [format!("extgate-{binding}")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(inbound, 1);
    let reply: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE session_id = ?1 AND content = ?2",
            [format!("extgate-{binding}"), REPLY.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reply, 1);
    drop(gw);
    drop(core);
    let _ = std::fs::remove_file(&sock);
}
