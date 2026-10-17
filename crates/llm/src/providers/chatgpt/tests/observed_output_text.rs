// D-1083: the observed call forwards first-item output text while the SSE body is arriving.

/// One HTTP response of the scripted mock: head, first body chunk, an optional hold, then the rest.
struct ObservedMockResponse {
    status_line: &'static str,
    content_length: Option<usize>,
    first: String,
    hold: Option<tokio::sync::oneshot::Receiver<()>>,
    rest: String,
}

impl ObservedMockResponse {
    fn sse(first: &str, rest: &str) -> Self {
        Self {
            status_line: "200 OK",
            content_length: None,
            first: first.to_string(),
            hold: None,
            rest: rest.to_string(),
        }
    }
}

async fn read_observed_mock_request(sock: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let Ok(n) = sock.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(head_end) = text.find("\r\n\r\n") {
            let length = text[..head_end]
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if buf.len() >= head_end + 4 + length {
                return;
            }
        }
    }
}

/// Serves `responses` in order, one per connection, to `POST .../codex/responses`.
async fn spawn_observed_mock(responses: Vec<ObservedMockResponse>) -> String {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for response in responses {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            read_observed_mock_request(&mut sock).await;
            let length = response
                .content_length
                .map(|n| format!("content-length: {n}\r\n"))
                .unwrap_or_default();
            let head = format!(
                "HTTP/1.1 {}\r\ncontent-type: text/event-stream\r\n{length}connection: close\r\n\r\n",
                response.status_line
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(response.first.as_bytes()).await;
            let _ = sock.flush().await;
            if let Some(hold) = response.hold {
                let _ = hold.await;
            }
            let _ = sock.write_all(response.rest.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    format!("http://{addr}")
}

fn delta_line(text: &str) -> String {
    format!(
        "event: response.output_text.delta\ndata: {}\n\n",
        serde_json::json!({"type": "response.output_text.delta", "delta": text})
    )
}

fn item_done_line(item: Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({"type": "response.output_item.done", "item": item})
    )
}

fn completed_line(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"type": "response.completed", "response": {
            "id": "resp-observed",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
            "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
        }})
    )
}

fn observed_provider(base_url: String, dir: &tempfile::TempDir) -> ChatGptProvider {
    let auth_file = write_auth_file(dir, &fake_jwt(3600), "rt");
    ChatGptProvider::new()
        .with_auth_file(&auth_file)
        .with_base_url(base_url)
}

fn collecting_sink() -> (OutputTextSink, tokio::sync::mpsc::UnboundedReceiver<OutputTextEvent>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let sink: OutputTextSink = std::sync::Arc::new(move |event| {
        let _ = tx.send(event);
    });
    (sink, rx)
}

fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<OutputTextEvent>) -> Vec<OutputTextEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn chat_request() -> ChatRequest {
    ChatRequest::new("gpt-5.6-sol", vec![Message::user("hi")])
}

/// C1: the delta reaches the sink while the server still holds the rest of the body.
#[tokio::test]
async fn observed_call_forwards_delta_before_body_completes() {
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let mut response = ObservedMockResponse::sse(
        &delta_line("こんにちは。"),
        &format!("{}{}", delta_line("元気？"), completed_line("こんにちは。元気？")),
    );
    response.hold = Some(release_rx);
    let base_url = spawn_observed_mock(vec![response]).await;
    let dir = tempfile::tempdir().unwrap();
    let provider = observed_provider(base_url, &dir);
    let (sink, mut rx) = collecting_sink();

    let call = tokio::spawn(async move {
        provider
            .chat_completion_with_history_observed(chat_request(), sink)
            .await
    });
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("delta must arrive while the body is still held");
    assert_eq!(first, Some(OutputTextEvent::Delta("こんにちは。".into())));
    assert!(!call.is_finished(), "the call must still be reading the body");

    release_tx.send(()).unwrap();
    let exchange = call.await.unwrap().unwrap();
    assert_eq!(drain(&mut rx), vec![OutputTextEvent::Delta("元気？".into())]);
    assert_eq!(
        exchange.response.choices[0].message.text_content(),
        Some("こんにちは。元気？")
    );
}

/// C1 GREEN: the observed exchange equals `parse_exchange` of the same SSE body.
#[tokio::test]
async fn observed_exchange_is_identical_to_buffered_parse() {
    let sse = format!(
        "{}{}{}{}{}",
        item_done_line(serde_json::json!({"type": "reasoning", "id": "rs_1"})),
        delta_line("見ます。"),
        item_done_line(serde_json::json!({"type": "message", "id": "msg_1"})),
        item_done_line(serde_json::json!({
            "type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":\"x\"}"
        })),
        completed_line("見ます。"),
    );
    let base_url = spawn_observed_mock(vec![ObservedMockResponse::sse(&sse, "")]).await;
    let dir = tempfile::tempdir().unwrap();
    let provider = observed_provider(base_url, &dir);
    let (sink, mut rx) = collecting_sink();

    let observed = provider
        .chat_completion_with_history_observed(chat_request(), sink)
        .await
        .unwrap();
    let buffered = provider.parse_exchange(&sse, "gpt-5.6-sol", false).unwrap();
    assert_eq!(
        serde_json::to_value(&observed).unwrap(),
        serde_json::to_value(&buffered).unwrap()
    );
    assert_eq!(drain(&mut rx), vec![OutputTextEvent::Delta("見ます。".into())]);
}

/// C1 GREEN: non-2xx keeps the full body in `api_error` and forwards nothing.
#[tokio::test]
async fn observed_non_success_returns_full_body_and_forwards_nothing() {
    let body = delta_line("見せない。");
    let response = ObservedMockResponse {
        status_line: "500 Internal Server Error",
        content_length: Some(body.len()),
        first: body.clone(),
        hold: None,
        rest: String::new(),
    };
    let base_url = spawn_observed_mock(vec![response]).await;
    let dir = tempfile::tempdir().unwrap();
    let provider = observed_provider(base_url, &dir);
    let (sink, mut rx) = collecting_sink();

    let err = provider
        .chat_completion_with_history_observed(chat_request(), sink)
        .await
        .unwrap_err();
    let llm = err.downcast_ref::<crate::LlmError>().expect("typed api_error");
    assert_eq!(llm.status(), Some(500));
    assert!(err.to_string().contains(body.trim_end()), "{err}");
    assert!(drain(&mut rx).is_empty());
}

/// C1 GREEN: a 401 refreshes once and the retried 2xx body is observed.
#[tokio::test]
async fn observed_call_refreshes_once_on_401() {
    let unauthorized = ObservedMockResponse {
        status_line: "401 Unauthorized",
        content_length: Some(2),
        first: "{}".into(),
        hold: None,
        rest: String::new(),
    };
    let ok = ObservedMockResponse::sse(
        &format!("{}{}", delta_line("どうぞ。"), completed_line("どうぞ。")),
        "",
    );
    let base_url = spawn_observed_mock(vec![unauthorized, ok]).await;
    let oauth_url = spawn_oauth_mock(
        "200 OK",
        serde_json::json!({"access_token": fake_jwt(7200)}).to_string(),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let provider = observed_provider(base_url, &dir).with_oauth_token_url(oauth_url);
    let (sink, mut rx) = collecting_sink();

    let exchange = provider
        .chat_completion_with_history_observed(chat_request(), sink)
        .await
        .unwrap();
    assert_eq!(exchange.response.id, "resp-observed");
    assert_eq!(drain(&mut rx), vec![OutputTextEvent::Delta("どうぞ。".into())]);
}

/// C1 GREEN: a body cut short is a read error, as in the buffered path.
#[tokio::test]
async fn observed_body_read_error_fails_the_call() {
    let first = delta_line("途中。");
    let response = ObservedMockResponse {
        status_line: "200 OK",
        content_length: Some(first.len() + 500),
        first,
        hold: None,
        rest: String::new(),
    };
    let base_url = spawn_observed_mock(vec![response]).await;
    let dir = tempfile::tempdir().unwrap();
    let provider = observed_provider(base_url, &dir);
    let (sink, _rx) = collecting_sink();

    let err = provider
        .chat_completion_with_history_observed(chat_request(), sink)
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("failed to read response body"),
        "{err:#}"
    );
}

/// C7b: items that finish before the first text (reasoning) do not freeze forwarding; the end
/// of the first text item does.
#[test]
fn observer_skips_items_before_first_text_and_freezes_after_it() {
    let sse = format!(
        "{}{}{}{}{}",
        item_done_line(serde_json::json!({"type": "reasoning", "id": "rs_1"})),
        delta_line("やあ。"),
        item_done_line(serde_json::json!({"type": "message", "id": "msg_1"})),
        delta_line("二つ目。"),
        completed_line("やあ。"),
    );
    let mut observer = super::observe::OutputTextObserver::new();
    let forwarded: String = sse
        .lines()
        .filter_map(|line| observer.observe_line(line))
        .collect();
    assert_eq!(forwarded, "やあ。");
}
