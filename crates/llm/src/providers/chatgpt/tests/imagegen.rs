// ---- 画像生成（D-1076）: POST {base_url}/codex/images/generations ----

/// 受けたリクエスト 1 本（小文字化したヘッダと本文）。
#[derive(Debug, Clone)]
struct CapturedRequest {
    request_line: String,
    headers: std::collections::HashMap<String, String>,
    body: String,
}

/// 台本の応答 1 本: (ステータス行, 追加ヘッダ, 本文)。
type ScriptedResponse = (&'static str, Vec<(&'static str, String)>, String);

/// 台本どおりの応答を順に返し、受けたリクエストを記録する HTTP モック。
/// 台本を使い切った後の接続には 500 を返す（余計な再試行を検出するため）。
async fn spawn_imagegen_mock(
    script: Vec<ScriptedResponse>,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<CapturedRequest>>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = captured.clone();
    tokio::spawn(async move {
        let mut script = script.into_iter();
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let (head_end, content_length) = loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break (raw.len(), 0);
                }
                raw.extend_from_slice(&buf[..n]);
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..pos]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (pos + 4, len);
                }
            };
            while raw.len() < head_end + content_length {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
            }
            let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
            let mut lines = head.lines();
            let request_line = lines.next().unwrap_or_default().to_string();
            let headers = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                .collect();
            let body = String::from_utf8_lossy(&raw[head_end..]).to_string();
            log.lock().unwrap().push(CapturedRequest {
                request_line,
                headers,
                body,
            });
            let (status, extra_headers, body) = script.next().unwrap_or((
                "500 Internal Server Error",
                vec![],
                r#"{"error":{"message":"unexpected extra request"}}"#.to_string(),
            ));
            let mut resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            for (k, v) in extra_headers {
                resp.push_str(&format!("{k}: {v}\r\n"));
            }
            resp.push_str("\r\n");
            resp.push_str(&body);
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}/backend-api"), captured)
}

/// 1x1 PNG の base64（`data[0].b64_json` に入れる）。
const IMAGEGEN_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4//8/AAX+Av7czFnnAAAAAElFTkSuQmCC";

fn imagegen_success_body() -> String {
    serde_json::json!({
        "created": 1,
        "background": "opaque",
        "output_format": "png",
        "quality": "auto",
        "size": "1024x1024",
        "data": [{"b64_json": IMAGEGEN_PNG_B64, "revised_prompt": "a calm red cat"}],
        "usage": {"input_tokens": 10, "output_tokens": 20},
    })
    .to_string()
}

fn imagegen_request(prompt: &str, transparent: bool) -> crate::ImageGenerationRequest {
    crate::ImageGenerationRequest {
        prompt: prompt.to_string(),
        transparent_background: transparent,
    }
}

fn imagegen_provider(dir: &tempfile::TempDir, token: &str, base_url: &str) -> ChatGptProvider {
    ChatGptProvider::new()
        .with_auth_file(write_auth_file(dir, token, "rt"))
        .with_base_url(base_url)
        .with_oauth_token_url("http://127.0.0.1:1/unreachable".to_string())
}

#[tokio::test]
async fn generate_image_sends_codex_images_request_and_decodes_png() {
    let dir = tempfile::tempdir().unwrap();
    let token = fake_jwt(3600);
    let (base, captured) = spawn_imagegen_mock(vec![(
        "200 OK",
        vec![("x-codex-imagegen-request-id", "req-123".to_string())],
        imagegen_success_body(),
    )])
    .await;
    let provider = imagegen_provider(&dir, &token, &base);

    let img = provider
        .generate_image(&imagegen_request("a red cat", true))
        .await
        .expect("success");

    assert!(img.png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "decoded PNG");
    assert_eq!(img.revised_prompt.as_deref(), Some("a calm red cat"));
    assert_eq!(img.request_id.as_deref(), Some("req-123"));
    assert_eq!(img.usage.as_ref().unwrap()["output_tokens"], 20);

    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!(
        req.request_line,
        "POST /backend-api/codex/images/generations HTTP/1.1"
    );
    assert_eq!(req.headers["authorization"], format!("Bearer {token}"));
    assert_eq!(req.headers["chatgpt-account-id"], "acct-test");
    assert_eq!(req.headers["originator"], "pi");
    assert_eq!(req.headers["accept"], "application/json");
    assert!(req.headers["content-type"].starts_with("application/json"));
    let body: Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "model": "gpt-image-2",
            "prompt": "a red cat",
            "background": "transparent",
            "quality": "auto",
            "size": "auto",
        })
    );
}

#[tokio::test]
async fn generate_image_defaults_to_opaque_background() {
    let dir = tempfile::tempdir().unwrap();
    let (base, captured) =
        spawn_imagegen_mock(vec![("200 OK", vec![], imagegen_success_body())]).await;
    let provider = imagegen_provider(&dir, &fake_jwt(3600), &base);
    provider
        .generate_image(&imagegen_request("p", false))
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&captured.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["background"], "opaque");
}

#[tokio::test]
async fn generate_image_refreshes_once_on_401_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let revoked = fake_jwt(3600);
    let fresh = fake_jwt(7200);
    let oauth = spawn_oauth_mock(
        "200 OK",
        serde_json::json!({"access_token": fresh}).to_string(),
    )
    .await;
    let (base, captured) = spawn_imagegen_mock(vec![
        (
            "401 Unauthorized",
            vec![],
            r#"{"error":{"message":"expired"}}"#.to_string(),
        ),
        ("200 OK", vec![], imagegen_success_body()),
    ])
    .await;
    let provider = imagegen_provider(&dir, &revoked, &base).with_oauth_token_url(oauth);

    let img = provider
        .generate_image(&imagegen_request("p", false))
        .await
        .expect("retry after refresh succeeds");
    assert!(!img.png_bytes.is_empty());

    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2, "exactly one retry");
    assert_eq!(reqs[0].headers["authorization"], format!("Bearer {revoked}"));
    assert_eq!(reqs[1].headers["authorization"], format!("Bearer {fresh}"));
}

#[tokio::test]
async fn generate_image_429_is_usage_limit_with_resets_at_and_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let (base, captured) = spawn_imagegen_mock(vec![(
        "429 Too Many Requests",
        vec![],
        serde_json::json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "resets_at": 1_900_000_000_i64,
            }
        })
        .to_string(),
    )])
    .await;
    let provider = imagegen_provider(&dir, &fake_jwt(3600), &base);

    let err = provider
        .generate_image(&imagegen_request("p", false))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        crate::ImageGenerationError::UsageLimit {
            message: "The usage limit has been reached".to_string(),
            resets_at: Some(1_900_000_000),
        }
    );
    assert_eq!(captured.lock().unwrap().len(), 1, "429 must not be retried");
}

#[tokio::test]
async fn generate_image_moderation_4xx_is_rejected_with_server_message() {
    let dir = tempfile::tempdir().unwrap();
    let (base, captured) = spawn_imagegen_mock(vec![(
        "400 Bad Request",
        vec![],
        serde_json::json!({
            "error": {
                "code": "moderation_blocked",
                "message": "Your request was rejected by the safety system.",
            }
        })
        .to_string(),
    )])
    .await;
    let provider = imagegen_provider(&dir, &fake_jwt(3600), &base);

    let err = provider
        .generate_image(&imagegen_request("p", false))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        crate::ImageGenerationError::Rejected {
            status: 400,
            message: "Your request was rejected by the safety system.".to_string(),
        }
    );
    assert_eq!(captured.lock().unwrap().len(), 1, "4xx must not be retried");
}

#[tokio::test]
async fn generate_image_empty_data_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (base, _captured) = spawn_imagegen_mock(vec![(
        "200 OK",
        vec![],
        r#"{"created":1,"data":[]}"#.to_string(),
    )])
    .await;
    let provider = imagegen_provider(&dir, &fake_jwt(3600), &base);

    let err = provider
        .generate_image(&imagegen_request("p", false))
        .await
        .unwrap_err();
    assert_eq!(err, crate::ImageGenerationError::EmptyData);
}

/// 画像生成は 1〜3 分かかるので専用クライアント（300 秒）を使い、チャットの
/// read timeout（既定 60 秒 / `with_timeout_secs`）は変えない。
#[test]
fn imagegen_client_timeout_is_independent_of_chat_timeout() {
    assert_eq!(super::imagegen::IMAGEGEN_TIMEOUT_SECS, 300);
    let p = ChatGptProvider::new().with_timeout_secs(5);
    assert_eq!(p.timeout_secs, 5, "chat timeout unchanged");
    assert_eq!(p.imagegen_timeout_secs, 300);
}

/// タイムアウトは `Timeout` に分類される（専用クライアントの timeout を短くして検証）。
#[tokio::test]
async fn generate_image_timeout_is_classified() {
    let dir = tempfile::tempdir().unwrap();
    let url = spawn_slow_mock(Duration::from_millis(1500)).await;
    let base = url.trim_end_matches("/slow").to_string();
    let provider = imagegen_provider(&dir, &fake_jwt(3600), &base).with_imagegen_timeout_secs(1);
    let err = provider
        .generate_image(&imagegen_request("p", false))
        .await
        .unwrap_err();
    assert_eq!(err, crate::ImageGenerationError::Timeout(1));
}
