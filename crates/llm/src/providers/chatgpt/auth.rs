use super::*;

/// OAuth トークンリフレッシュのエンドポイント（codex CLI と同じ）。
pub(super) const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// codex CLI の公開 OAuth client_id（openai/codex リポジトリの定数と同一。
/// 秘密情報ではない — PKCE フローのパブリッククライアント識別子）。
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// access_token の失効をこの秒数だけ前倒しで判定する（リクエスト飛行中の失効を防ぐ）。
const TOKEN_EXPIRY_MARGIN_SECS: i64 = 60;
/// チャット補完リクエスト全体（＝生成の読み切り）の timeout 既定値（秒）。
/// `reasoning_effort` を上げると 1 ターンの生成がこれを超えることがあるので、
/// config の `timeout_secs` で伸ばせる（#433）。
pub(super) const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// 接続確立の timeout（秒）。生成の長さとは無関係なので config では変えない。
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// リフレッシュの直列化（同時多発の 401 で refresh_token を並行消費しない）。
/// OpenAI はリフレッシュでトークンをローテーションするため、並行リフレッシュは
/// 古い refresh_token の再利用 = 失敗になりうる。プロセス全体で 1 本に絞る。
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Expand a leading `~` in a path to the value of the `HOME` environment variable.
pub(super) fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/{}", home, rest)
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else {
        path.to_string()
    }
}

/// Decode a base64url string (no padding required) into bytes.
pub(super) fn base64url_decode(input: &str) -> anyhow::Result<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &c in input.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = val(c).ok_or_else(|| anyhow::anyhow!("invalid base64url character"))? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

/// JWT のペイロード部（2 番目のセグメント）を JSON として取り出す。
fn jwt_payload(token: &str) -> anyhow::Result<serde_json::Value> {
    use anyhow::Context;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        anyhow::bail!("invalid JWT: expected at least 2 dot-separated parts");
    }
    let payload_bytes =
        base64url_decode(parts[1]).context("failed to base64url-decode JWT payload")?;
    serde_json::from_slice(&payload_bytes).context("failed to parse JWT payload JSON")
}

/// Extract the `chatgpt_account_id` from a JWT access token's claims.
pub(super) fn extract_account_id(token: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let payload = jwt_payload(token)?;
    let account_id = payload["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .context("chatgpt_account_id not found in JWT claims")?
        .to_string();
    Ok(account_id)
}

/// access_token が失効している（または間もなく失効する）か。
/// exp クレームが読めない場合は false（判定不能なら 401 リトライ側に任せる）。
pub(super) fn token_expired(token: &str) -> bool {
    let Ok(payload) = jwt_payload(token) else {
        return false;
    };
    let Some(exp) = payload["exp"].as_i64() else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    exp - TOKEN_EXPIRY_MARGIN_SECS <= now
}

/// チャット補完用の HTTP クライアントを組む。read timeout だけが可変。
pub(super) fn build_client(timeout_secs: u64) -> Client {
    Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .build()
        .unwrap_or_default()
}

impl ChatGptProvider {
    /// Read access_token from auth_file
    pub(super) fn load_access_token(&self) -> Result<String> {
        let token = self.load_auth_json()?["tokens"]["access_token"]
            .as_str()
            .context("tokens.access_token not found in auth.json")?
            .to_string();
        Ok(token)
    }

    fn load_auth_json(&self) -> Result<Value> {
        let content = std::fs::read_to_string(&self.auth_file)
            .with_context(|| format!("Failed to read auth file: {}", self.auth_file))?;
        serde_json::from_str(&content).context("Failed to parse auth.json")
    }

    /// 有効な access_token を返す。失効している（60 秒以内に失効する）場合は
    /// リフレッシュしてから返す。
    ///
    /// これまでは auth.json を読むだけだったため、codex CLI を手動実行して
    /// auth.json が書き換わらない限り、access_token の失効とともに全呼び出しが
    /// 401 で沈黙していた（bot はリアクションだけ返して返信しない症状になる）。
    pub(super) async fn fresh_access_token(&self) -> Result<String> {
        let token = self.load_access_token()?;
        if !token_expired(&token) {
            return Ok(token);
        }
        tracing::info!("ChatGPT access token expired; refreshing");
        self.refresh_access_token(Some(&token)).await
    }

    /// refresh_token で access_token を更新し、auth.json へ永続化して新トークンを返す。
    ///
    /// `stale_token` は「使えないと分かっているトークン」（exp 失効 or 401 を返した
    /// トークン）。auth.json の現在値がこれと**異なり**かつ exp 有効なら、他タスク/
    /// 他プロセスが更新済みなのでそれを返す。同一なら exp 上は有効でも（サーバ側
    /// 取り消し等）実リフレッシュへ進む。
    ///
    /// codex CLI と同じ auth.json を更新するため、書き込みは同一ディレクトリの
    /// 一意な一時ファイル + rename で原子的に行い、パーミッションは元ファイルを
    /// 引き継ぐ（トークンを含むファイルを 0644 に緩めない）。
    pub(super) async fn refresh_access_token(&self, stale_token: Option<&str>) -> Result<String> {
        let _guard = REFRESH_LOCK.lock().await;

        // ロック待ちの間に他タスク/他プロセスがリフレッシュ済みかもしれない — 再読して確認
        let mut auth = self.load_auth_json()?;
        let started_with = auth["tokens"]["access_token"]
            .as_str()
            .map(|s| s.to_string());
        if let Some(current) = started_with.as_deref() {
            if !token_expired(current) && stale_token != Some(current) {
                return Ok(current.to_string());
            }
        }
        let refresh_token = auth["tokens"]["refresh_token"]
            .as_str()
            .context("tokens.refresh_token not found in auth.json — run `codex login` once")?
            .to_string();

        let resp = self
            .client
            .post(&self.oauth_token_url)
            .json(&serde_json::json!({
                "client_id": CODEX_OAUTH_CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "scope": "openid profile email",
            }))
            .send()
            .await
            .context("ChatGPT token refresh request failed")?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .context("ChatGPT token refresh: failed to read response body")?;
        if !status.is_success() {
            // codex CLI 等の別プロセスが並行リフレッシュして refresh_token を
            // ローテーション済みだと invalid_grant になる。auth.json が外部更新
            // されていればそれで自己回復する（誤った「codex login せよ」を出さない）。
            if let Ok(latest) = self.load_auth_json() {
                if let Some(current) = latest["tokens"]["access_token"].as_str() {
                    if Some(current) != started_with.as_deref() && !token_expired(current) {
                        tracing::info!(
                            "ChatGPT token refresh failed but auth.json was updated externally; using the new token"
                        );
                        return Ok(current.to_string());
                    }
                }
            }
            // refresh_token 自体が失効/取り消しされたケース。自動では復旧できない。
            anyhow::bail!(
                "ChatGPT token refresh failed ({status}): {text} — run `codex login` to re-authenticate"
            );
        }
        let parsed: Value =
            serde_json::from_str(&text).context("ChatGPT token refresh: invalid JSON response")?;
        let new_access = parsed["access_token"]
            .as_str()
            .context("ChatGPT token refresh: access_token missing in response")?
            .to_string();

        // トークン一式を auth.json に書き戻す（他のフィールドは保存）。
        auth["tokens"]["access_token"] = Value::String(new_access.clone());
        if let Some(new_refresh) = parsed["refresh_token"].as_str() {
            auth["tokens"]["refresh_token"] = Value::String(new_refresh.to_string());
        }
        if let Some(new_id) = parsed["id_token"].as_str() {
            auth["tokens"]["id_token"] = Value::String(new_id.to_string());
        }
        auth["last_refresh"] = Value::String(chrono::Utc::now().to_rfc3339());

        let serialized = serde_json::to_string_pretty(&auth)
            .context("ChatGPT token refresh: failed to serialize auth.json")?;
        let auth_path = std::path::Path::new(&self.auth_file);
        let dir = auth_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        // tempfile は一意名 + 0600 で作られる（固定名 .tmp のプロセス間衝突と
        // umask 由来の 0644 化を両方回避）。元ファイルのモードがあれば引き継ぐ。
        let mut tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("failed to create temp file in {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(auth_path)
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0o600);
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(mode))
                .context("failed to set temp file permissions")?;
        }
        {
            use std::io::Write as _;
            tmp.write_all(serialized.as_bytes())
                .context("failed to write refreshed auth.json")?;
        }
        tmp.persist(auth_path)
            .with_context(|| format!("failed to replace {}", auth_path.display()))?;

        tracing::info!("ChatGPT access token refreshed and persisted to auth.json");
        Ok(new_access)
    }
}
