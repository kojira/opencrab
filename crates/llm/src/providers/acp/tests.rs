use super::*;

#[test]
fn test_finish_reason_from_stop() {
    assert!(matches!(
        finish_reason_from_stop("end_turn"),
        FinishReason::Stop
    ));
    assert!(matches!(
        finish_reason_from_stop("max_tokens"),
        FinishReason::Length
    ));
    assert!(matches!(
        finish_reason_from_stop("refusal"),
        FinishReason::ContentFilter
    ));
    assert!(matches!(
        finish_reason_from_stop("cancelled"),
        FinishReason::Stop
    ));
    assert!(matches!(
        finish_reason_from_stop("weird"),
        FinishReason::Stop
    ));
}

#[test]
fn test_pick_permission_option() {
    let opts = json!([
        {"optionId":"r","name":"Reject","kind":"reject_once"},
        {"optionId":"a","name":"Allow","kind":"allow_once"}
    ]);
    // 許可肢を優先。
    assert_eq!(pick_permission_option(&opts), Some("a".to_string()));
    // 許可肢が無ければ reject。
    let only_reject = json!([{"optionId":"r","name":"Reject","kind":"reject_always"}]);
    assert_eq!(pick_permission_option(&only_reject), Some("r".to_string()));
    // 空/不正は None。
    assert_eq!(pick_permission_option(&json!([])), None);
    assert_eq!(pick_permission_option(&json!("x")), None);
}

#[test]
fn test_accumulate_update() {
    let text = Arc::new(Mutex::new(String::new()));
    let mk = |t: &str| json!({"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text": t}}}});
    accumulate_update(&mk("Hello "), &text);
    accumulate_update(&mk("world"), &text);
    // thought は無視。
    accumulate_update(
        &json!({"method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}}}),
        &text,
    );
    assert_eq!(*text.lock().unwrap(), "Hello world");
}

#[test]
fn test_builder_and_name() {
    let p = AcpProvider::new()
        .with_binary_path("gemini")
        .with_args(vec!["--experimental-acp".to_string()])
        .with_default_model("gemini-2.5-pro")
        .with_timeout_secs(120);
    assert_eq!(p.name(), "acp");
    assert_eq!(p.binary_path, "gemini");
    assert_eq!(p.args, vec!["--experimental-acp"]);
    assert_eq!(p.default_model, "gemini-2.5-pro");
    assert_eq!(p.timeout, Duration::from_secs(120));
    assert!(!p.supports_function_calling());
    // 空指定は無視される。
    let p2 = AcpProvider::new().with_binary_path("").with_timeout_secs(0);
    assert_eq!(p2.binary_path, DEFAULT_ACP_PATH);
    assert_eq!(p2.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
}

/// in-memory パイプでモック ACP エージェントを立て、initialize→new→prompt と、
/// ターン中の session/request_permission 応答、agent_message_chunk 蓄積を検証する。
#[tokio::test]
async fn test_drive_session_over_duplex() {
    let (client_w, agent_r) = tokio::io::duplex(16384);
    let (agent_w, client_r) = tokio::io::duplex(16384);

    // モックエージェント。
    tokio::spawn(async move {
        let mut lines = BufReader::new(agent_r).lines();
        let mut w = agent_w;
        let mut permission_answered = false;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let id = msg.get("id").cloned();
            // これは client → agent の応答（permission への返答）。
            if method.is_empty() && msg.get("result").is_some() {
                permission_answered = true;
                continue;
            }
            let result = match method {
                "initialize" => {
                    Some(json!({"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}))
                }
                "session/new" => Some(json!({"sessionId":"sess-1"})),
                "session/prompt" => {
                    // まず permission を要求 → 本文チャンク → 応答（end_turn）。
                    // 文字列 id で送る（JSON-RPC 2.0 は文字列 id を許す。数値決め打ちで
                    // 誤分類→未応答→ハングしないことの回帰テスト）。
                    let perm = json!({"jsonrpc":"2.0","id":"perm-1","method":"session/request_permission","params":{"sessionId":"sess-1","toolCall":{"toolCallId":"t1"},"options":[{"optionId":"ok","name":"Allow","kind":"allow_once"}]}});
                    let mut s = serde_json::to_string(&perm).unwrap();
                    s.push('\n');
                    let _ = w.write_all(s.as_bytes()).await;
                    let _ = w.flush().await;
                    // client の応答を待つ（同ループで permission_answered が立つ）。
                    for _ in 0..50 {
                        if permission_answered {
                            break;
                        }
                        if let Ok(Some(l)) = lines.next_line().await {
                            if let Ok(m) = serde_json::from_str::<Value>(&l) {
                                if m.get("result").is_some() && m.get("method").is_none() {
                                    permission_answered = true;
                                }
                            }
                        }
                    }
                    for chunk in ["Hi", " there"] {
                        let upd = json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":chunk}}}});
                        let mut s = serde_json::to_string(&upd).unwrap();
                        s.push('\n');
                        let _ = w.write_all(s.as_bytes()).await;
                        let _ = w.flush().await;
                    }
                    Some(json!({"stopReason":"end_turn"}))
                }
                _ => None,
            };
            if let (Some(id), Some(result)) = (id, result) {
                let resp = json!({"jsonrpc":"2.0","id":id,"result":result});
                let mut s = serde_json::to_string(&resp).unwrap();
                s.push('\n');
                let _ = w.write_all(s.as_bytes()).await;
                let _ = w.flush().await;
            }
        }
    });

    let (text, stop) = drive_acp_session(
        Box::new(client_w),
        client_r,
        "hello".to_string(),
        "/tmp".to_string(),
    )
    .await
    .unwrap();
    assert_eq!(text, "Hi there");
    assert_eq!(stop, "end_turn");
}

#[tokio::test]
async fn test_health_handshake_ok_when_initialize_answered() {
    let (client_w, agent_r) = tokio::io::duplex(8192);
    let (agent_w, client_r) = tokio::io::duplex(8192);
    // initialize に応答するモックエージェント。
    tokio::spawn(async move {
        let mut lines = BufReader::new(agent_r).lines();
        let mut w = agent_w;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg.get("method").and_then(|v| v.as_str()) == Some("initialize") {
                let id = msg.get("id").cloned().unwrap_or(json!(1));
                let resp = json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"authMethods":[]}});
                let mut s = serde_json::to_string(&resp).unwrap();
                s.push('\n');
                let _ = w.write_all(s.as_bytes()).await;
                let _ = w.flush().await;
            }
        }
    });
    assert!(acp_initialize_handshake(Box::new(client_w), client_r)
        .await
        .is_ok());
}

#[test]
fn test_stderr_suffix_empty_and_populated() {
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    assert_eq!(stderr_suffix(&tail), "");
    tail.lock().unwrap().push_back("boom".to_string());
    let s = stderr_suffix(&tail);
    assert!(s.contains("stderr"));
    assert!(s.contains("boom"));
}

#[tokio::test]
async fn test_stderr_tail_captures_lines() {
    // 実サブプロセスの stderr を drain して末尾に取り込めること。
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("echo err1 >&2; echo err2 >&2")
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let tail = spawn_stderr_tail(child.stderr.take().unwrap());
    let _ = child.wait().await;
    // drain タスクが読み切るのを待つ。
    for _ in 0..50 {
        if tail.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let s = stderr_suffix(&tail);
    assert!(s.contains("err1"), "stderr tail should contain err1: {s}");
    assert!(s.contains("err2"), "stderr tail should contain err2: {s}");
}

#[tokio::test]
async fn test_health_handshake_fails_when_connection_closes() {
    let (client_w, agent_r) = tokio::io::duplex(8192);
    let (agent_w, client_r) = tokio::io::duplex(8192);
    // 応答せず即クローズ（`npx --version` は通るが ACP を話せないエージェントの模擬）。
    drop(agent_r);
    drop(agent_w);
    assert!(acp_initialize_handshake(Box::new(client_w), client_r)
        .await
        .is_err());
}
