use super::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// モックが記録した (content-type, body) の列。
type Recorded = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// 依存を増やさない最小の HTTP モック。実 Discord には一切出さない。
/// 受け取った (content-type, body) を記録し、`delay` 後に 204 を返す。
async fn mock_webhook(delay: Duration) -> (String, Recorded) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let got: Recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = got.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let sink = sink.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let head_end = loop {
                    let n = match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                let header = |name: &str| -> Option<String> {
                    head.split("\r\n").find_map(|l| {
                        l.strip_prefix(&format!("{name}: "))
                            .map(|v| v.trim().to_string())
                    })
                };
                let len: usize = header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                sink.lock().unwrap().push((
                    header("content-type").unwrap_or_default(),
                    buf[head_end..].to_vec(),
                ));
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let _ = stream
                    .write_all(b"HTTP/1.1 204 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://{addr}/api/webhooks/1/tok"), got)
}

async fn wait_for(got: &Recorded, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if got.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {n} request(s), got {}", got.lock().unwrap().len());
}

/// #293: 長い転記本文は分割連投せず、**1 回の multipart** で出る。
/// allowed_mentions の抑止は payload_json 側に載ったままであること（#252 の担保）。
#[tokio::test]
async fn long_relay_text_is_one_multipart_post() {
    let (url, got) = mock_webhook(Duration::ZERO).await;
    let text = "N".repeat(6000);
    let msg = opencrab_actions::build_message_with_optional_attachment(&text, "nostr-inbound");
    spawn_relay_post(url, msg);
    wait_for(&got, 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let reqs = got.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1, "長文でも POST は 1 回だけ");
    assert!(
        reqs[0].0.starts_with("multipart/form-data"),
        "content-type: {}",
        reqs[0].0
    );
    let body = String::from_utf8_lossy(&reqs[0].1).to_string();
    assert!(body.contains("filename=\"nostr-inbound.txt\""));
    assert!(body.contains(&text), "添付が全文でない");
    // mention 暴発の抑止は multipart でも維持される。
    assert!(body.contains("allowed_mentions"), "mention 抑止が落ちた");
}

/// 短い転記は従来どおり JSON のみ（添付しない）。回帰テスト。
#[tokio::test]
async fn short_relay_text_stays_plain_json() {
    let (url, got) = mock_webhook(Duration::ZERO).await;
    let msg = opencrab_actions::build_message_with_optional_attachment("hi", "nostr-inbound");
    spawn_relay_post(url, msg);
    wait_for(&got, 1).await;
    let reqs = got.lock().unwrap().clone();
    assert_eq!(reqs[0].0, "application/json");
    let body = String::from_utf8_lossy(&reqs[0].1).to_string();
    assert!(body.contains(r#""content":"hi""#), "body: {body}");
    assert!(body.contains("allowed_mentions"));
}

/// 相手が遅くても呼び出し元（Nostr 受信ループ）は即座に戻る。
#[tokio::test]
async fn relay_post_never_blocks_the_caller() {
    let slow = Duration::from_millis(600);
    let (url, got) = mock_webhook(slow).await;
    let msg = opencrab_actions::build_message_with_optional_attachment(
        &"S".repeat(5000),
        "nostr-inbound",
    );
    let start = Instant::now();
    spawn_relay_post(url, msg);
    assert!(
        start.elapsed() < slow,
        "呼び出し元が配送に引きずられた: {:?}",
        start.elapsed()
    );
    wait_for(&got, 1).await;
}

/// 送信が失敗（宛先が居ない）しても panic せず、呼び出し元の後続処理は進む。
#[tokio::test]
async fn relay_post_failure_is_swallowed() {
    // 誰も listen していないポートへ投げる。
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let msg = opencrab_actions::build_message_with_optional_attachment("boom", "nostr-inbound");
    spawn_relay_post(format!("http://{addr}/api/webhooks/1/tok"), msg);
    // 呼び出し元はそのまま進める。
    tokio::time::sleep(Duration::from_millis(300)).await;
}
