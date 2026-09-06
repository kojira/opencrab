use std::io::Write;
use tempfile::NamedTempFile;

fn b64url_encode(data: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 63) as usize] as char);
        out.push(ALPHA[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHA[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHA[(n & 63) as usize] as char);
        }
    }
    out
}

#[test]
fn test_expand_tilde_basic() {
    let home = std::env::var("HOME").unwrap_or_default();
    assert_eq!(
        expand_tilde("~/.codex/auth.json"),
        format!("{}/.codex/auth.json", home)
    );
    assert_eq!(expand_tilde("~"), home);
    assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
    assert_eq!(expand_tilde("relative/path"), "relative/path");
}

#[test]
fn test_parse_auth_json() {
    let mut file = NamedTempFile::new().expect("failed to create temp file");
    write!(file, r#"{{"tokens":{{"access_token":"test-token-123"}}}}"#)
        .expect("failed to write temp file");
    let path = file.path().to_str().expect("invalid temp path").to_string();
    let provider = ChatGptProvider::new().with_auth_file(path);
    let token = provider.load_access_token();
    assert_eq!(token.unwrap(), "test-token-123");
}

#[test]
fn test_load_access_token_missing_file() {
    let provider = ChatGptProvider::new().with_auth_file("/nonexistent/path/auth.json");
    assert!(provider.load_access_token().is_err());
}

#[test]
fn test_base64url_decode_roundtrip() {
    let samples: &[&[u8]] = &[b"", b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"];
    for s in samples {
        let encoded = b64url_encode(s);
        assert_eq!(base64url_decode(&encoded).unwrap(), s.to_vec());
    }
}

#[test]
fn test_extract_account_id() {
    let payload = serde_json::json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": "acct-xyz-123" }
    });
    let payload_b64 = b64url_encode(payload.to_string().as_bytes());
    let token = format!("header.{}.signature", payload_b64);
    assert_eq!(extract_account_id(&token).unwrap(), "acct-xyz-123");
}

#[test]
fn test_extract_account_id_invalid() {
    assert!(extract_account_id("notajwt").is_err());
}

// ---- トークンリフレッシュ（#失効で bot が沈黙する問題の修正）----

/// テスト用 JWT を作る（署名は検証しないので偽物で良い）。
fn fake_jwt(exp_offset_secs: i64) -> String {
    fn b64url(data: &[u8]) -> String {
        const CHARS: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                chunk.get(1).copied().unwrap_or(0),
                chunk.get(2).copied().unwrap_or(0),
            ];
            let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
            out.push(CHARS[(n >> 18) as usize & 63] as char);
            out.push(CHARS[(n >> 12) as usize & 63] as char);
            if chunk.len() > 1 {
                out.push(CHARS[(n >> 6) as usize & 63] as char);
            }
            if chunk.len() > 2 {
                out.push(CHARS[n as usize & 63] as char);
            }
        }
        out
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let payload = serde_json::json!({
        "exp": now + exp_offset_secs,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct-test"},
    });
    format!(
        "{}.{}.{}",
        b64url(b"{\"alg\":\"none\"}"),
        b64url(payload.to_string().as_bytes()),
        b64url(b"sig")
    )
}

#[test]
fn test_token_expired_by_exp_claim() {
    assert!(token_expired(&fake_jwt(-3600)), "past exp must be expired");
    // マージン（60s）内も失効扱い
    assert!(token_expired(&fake_jwt(30)));
    assert!(!token_expired(&fake_jwt(3600)));
    // exp が読めないトークンは false（401 リトライ側に任せる）
    assert!(!token_expired("not-a-jwt"));
}

/// リクエストを受けてから `delay` 待って 200 を返すモック（read timeout 検証用）。
async fn spawn_slow_mock(delay: Duration) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                tokio::time::sleep(delay).await;
                let resp =
                    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}/slow")
}

/// #433: read timeout は 60 秒ハードコードではなく、`with_timeout_secs` で伸ばせる。
/// 保持している値だけでなく、**実際に client へ効いている**ことまで見る。
#[tokio::test]
async fn test_timeout_secs_is_applied_to_the_http_client() {
    assert_eq!(
        ChatGptProvider::new().timeout_secs,
        DEFAULT_TIMEOUT_SECS,
        "未設定の既定は 60 秒のまま"
    );

    let url = spawn_slow_mock(Duration::from_millis(1500)).await;

    let short = ChatGptProvider::new().with_timeout_secs(1);
    let err = short.client.get(&url).send().await.unwrap_err();
    assert!(err.is_timeout(), "1 秒なら読み切る前に timeout する: {err}");

    let long = ChatGptProvider::new().with_timeout_secs(10);
    let resp = long
        .client
        .get(&url)
        .send()
        .await
        .expect("10 秒なら読み切れる");
    assert!(resp.status().is_success());
}

/// 1 接続だけ受けて固定レスポンスを返す極小 HTTP モック。
async fn spawn_oauth_mock(status_line: &'static str, body: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}/oauth/token")
}

fn write_auth_file(dir: &tempfile::TempDir, access: &str, refresh: &str) -> String {
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "OPENAI_API_KEY": "keep-me",
            "tokens": {
                "access_token": access,
                "refresh_token": refresh,
                "id_token": "old-id",
                "account_id": "acct-test",
            },
            "last_refresh": "2020-01-01T00:00:00Z",
        })
        .to_string(),
    )
    .unwrap();
    path.to_string_lossy().to_string()
}

#[tokio::test]
async fn test_refresh_on_expired_token_persists_auth_json() {
    let dir = tempfile::tempdir().unwrap();
    let new_token = fake_jwt(3600);
    let mock_url = spawn_oauth_mock(
        "200 OK",
        serde_json::json!({
            "access_token": new_token,
            "refresh_token": "rotated-rt",
            "id_token": "new-id",
        })
        .to_string(),
    )
    .await;
    let auth_file = write_auth_file(&dir, &fake_jwt(-3600), "old-rt");
    let provider = ChatGptProvider::new()
        .with_auth_file(&auth_file)
        .with_oauth_token_url(mock_url);

    let got = provider.fresh_access_token().await.unwrap();
    assert_eq!(got, new_token);

    // auth.json が更新され、無関係フィールドは保存される
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(&auth_file).unwrap()).unwrap();
    assert_eq!(saved["tokens"]["access_token"].as_str().unwrap(), new_token);
    assert_eq!(saved["tokens"]["refresh_token"], "rotated-rt");
    assert_eq!(saved["tokens"]["id_token"], "new-id");
    assert_eq!(saved["OPENAI_API_KEY"], "keep-me");
    assert_eq!(saved["tokens"]["account_id"], "acct-test");
    assert_ne!(saved["last_refresh"], "2020-01-01T00:00:00Z");

    // 2 回目は失効していないのでリフレッシュ不要（モックに触らず即返る）
    let again = provider.fresh_access_token().await.unwrap();
    assert_eq!(again, new_token);
}

#[tokio::test]
async fn test_fresh_token_skips_refresh_when_valid() {
    let dir = tempfile::tempdir().unwrap();
    let valid = fake_jwt(3600);
    // OAuth モックを立てない = リフレッシュが呼ばれたら接続エラーで失敗する
    let auth_file = write_auth_file(&dir, &valid, "rt");
    let provider = ChatGptProvider::new()
        .with_auth_file(&auth_file)
        .with_oauth_token_url("http://127.0.0.1:1/unreachable".to_string());
    assert_eq!(provider.fresh_access_token().await.unwrap(), valid);
}

#[tokio::test]
async fn test_refresh_failure_mentions_codex_login() {
    let dir = tempfile::tempdir().unwrap();
    let mock_url = spawn_oauth_mock(
        "400 Bad Request",
        r#"{"error":"invalid_grant"}"#.to_string(),
    )
    .await;
    let auth_file = write_auth_file(&dir, &fake_jwt(-3600), "revoked-rt");
    let provider = ChatGptProvider::new()
        .with_auth_file(&auth_file)
        .with_oauth_token_url(mock_url);
    let err = provider.fresh_access_token().await.unwrap_err().to_string();
    assert!(err.contains("codex login"), "err was: {err}");
    // 失敗時は auth.json を書き換えない
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(&auth_file).unwrap()).unwrap();
    assert_eq!(saved["tokens"]["refresh_token"], "revoked-rt");
}

#[tokio::test]
async fn test_reactive_refresh_works_for_revoked_but_unexpired_token() {
    // exp 上は有効でもサーバに拒否された（401 経路の）トークンは、stale 指定で
    // 実リフレッシュに進む（double-check の早期 return で素通りしない）
    let dir = tempfile::tempdir().unwrap();
    let revoked = fake_jwt(3600); // まだ exp 有効
    let new_token = fake_jwt(7200);
    let mock_url = spawn_oauth_mock(
        "200 OK",
        serde_json::json!({"access_token": new_token}).to_string(),
    )
    .await;
    let auth_file = write_auth_file(&dir, &revoked, "rt");
    let provider = ChatGptProvider::new()
        .with_auth_file(&auth_file)
        .with_oauth_token_url(mock_url);

    let got = provider.refresh_access_token(Some(&revoked)).await.unwrap();
    assert_eq!(got, new_token);
}

#[cfg(unix)]
#[tokio::test]
async fn test_refresh_preserves_auth_json_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mock_url = spawn_oauth_mock(
        "200 OK",
        serde_json::json!({"access_token": fake_jwt(3600)}).to_string(),
    )
    .await;
    let auth_file = write_auth_file(&dir, &fake_jwt(-3600), "rt");
    std::fs::set_permissions(&auth_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let provider = ChatGptProvider::new()
        .with_auth_file(&auth_file)
        .with_oauth_token_url(mock_url);

    provider.fresh_access_token().await.unwrap();
    let mode = std::fs::metadata(&auth_file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "auth.json permissions must not be loosened");
}
