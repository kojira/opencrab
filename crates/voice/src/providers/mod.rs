pub mod openai_stt;
pub mod openai_tts;
pub mod voicevox;

#[cfg(test)]
pub(crate) mod test_util {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 極小 HTTP モック。受けたリクエスト全文を保存し、固定レスポンスを返す。
    pub async fn spawn_http_mock(
        status_line: &'static str,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = captured.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = body.clone();
                let cap = cap.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 8192];
                    // Content-Length 分まで読む（multipart は複数 read になる）
                    while let Ok(n) = sock.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        req.extend_from_slice(&buf[..n]);
                        if let Some(pos) = find_headers_end(&req) {
                            let headers = String::from_utf8_lossy(&req[..pos]);
                            let clen = headers
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().ok())?
                                })
                                .unwrap_or(0);
                            if req.len() >= pos + 4 + clen {
                                break;
                            }
                        }
                    }
                    *cap.lock().unwrap() = req;
                    let resp = format!(
                        "HTTP/1.1 {status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), captured)
    }

    fn find_headers_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }
}
