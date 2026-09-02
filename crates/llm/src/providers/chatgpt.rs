use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const DEFAULT_MODEL: &str = "gpt-5.5";

/// 「読んだが黙る」を表すプロジェクト全体のセンチネル（下流は `trim() == NO_REPLY` で判定）。
/// #844: verbosity=medium だと 1 応答に message アイテムが複数（実測最大7・ほぼ同一）出て、
/// 「返答→NO_REPLY→返答」のロールプレイ軌跡形を取ることがある。これを無区切り連結すると
/// 下流の全文一致を素通りしてセンチネルが平文露出するため、parse_response で
/// アイテム境界を保持し、非センチネルの先頭アイテムを採用する（全アイテムがセンチネルなら
/// NO_REPLY を残す）。判定をこの集約直後の 1 箇所へ前倒しし、下流の全文一致は backstop に残す。
const NO_REPLY_SENTINEL: &str = "NO_REPLY";

/// OAuth トークンリフレッシュのエンドポイント（codex CLI と同じ）。
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// codex CLI の公開 OAuth client_id（openai/codex リポジトリの定数と同一。
/// 秘密情報ではない — PKCE フローのパブリッククライアント識別子）。
const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// access_token の失効をこの秒数だけ前倒しで判定する（リクエスト飛行中の失効を防ぐ）。
const TOKEN_EXPIRY_MARGIN_SECS: i64 = 60;
/// チャット補完リクエスト全体（＝生成の読み切り）の timeout 既定値（秒）。
/// `reasoning_effort` を上げると 1 ターンの生成がこれを超えることがあるので、
/// config の `timeout_secs` で伸ばせる（#433）。
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// 接続確立の timeout（秒）。生成の長さとは無関係なので config では変えない。
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// リフレッシュの直列化（同時多発の 401 で refresh_token を並行消費しない）。
/// OpenAI はリフレッシュでトークンをローテーションするため、並行リフレッシュは
/// 古い refresh_token の再利用 = 失敗になりうる。プロセス全体で 1 本に絞る。
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Expand a leading `~` in a path to the value of the `HOME` environment variable.
fn expand_tilde(path: &str) -> String {
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
fn base64url_decode(input: &str) -> anyhow::Result<Vec<u8>> {
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

/// バイト列を標準 base64（`+/`・パディングあり）で符号化する。data URI 用。
fn base64_encode(data: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// http(s) URL のホストを解決し、全解決 IP が公開アドレスであることを確認する（SSRF 対策）。
/// 接続に使う（検証済みの）SocketAddr を返す。1つでも非公開アドレスに解決したら拒否。
async fn validate_public_url(parsed: &reqwest::Url) -> anyhow::Result<std::net::SocketAddr> {
    use anyhow::Context;
    match parsed.scheme() {
        "http" | "https" => {}
        other => anyhow::bail!("unsupported url scheme for image fetch: {other}"),
    }
    let host = parsed.host_str().context("image url has no host")?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("failed to resolve host {host}"))?
        .collect();
    let first = *addrs
        .first()
        .context("host did not resolve to any address")?;
    for addr in &addrs {
        if !is_global_ip(addr.ip()) {
            anyhow::bail!(
                "refusing to fetch image from non-public address ({})",
                addr.ip()
            );
        }
    }
    Ok(first)
}

/// IP が公開（グローバル）アドレスか。ループバック/プライベート/リンクローカル
/// （169.254.169.254 のメタデータ含む）/CGNAT/ユニークローカル等は非公開として弾く。
/// `std::net` の `is_global` は unstable のため、既知の非公開レンジを直接判定する。
fn is_global_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local() // 169.254.0.0/16（メタデータ 169.254.169.254 含む）
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || (o[0] == 100 && (o[1] & 0xc0) == 64)) // 100.64.0.0/10 CGNAT
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped（::ffff:a.b.c.d）で内部アドレスへ回避されないよう展開して判定。
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_global_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                // 埋め込み v4 で内部アドレスを指しうる遷移レンジは一律非公開扱い
                // （::a.b.c.d 互換 / 6to4 / Teredo / NAT64。正当な画像ホストは通常来ない）。
                || (s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0) // ::/96 IPv4-compatible
                || s[0] == 0x2002 // 6to4 2002::/16
                || (s[0] == 0x2001 && s[1] == 0x0000) // Teredo 2001:0000::/32
                || (s[0] == 0x0064 && s[1] == 0xff9b)) // NAT64 64:ff9b::/96
        }
    }
}

/// URL の拡張子から画像 MIME を推測する（Content-Type が無い/不正なときのフォールバック）。
fn guess_image_mime(url: &str) -> String {
    // クエリ以降を落として拡張子を見る。
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png", // 不明時は png 扱い（多くの backend が受ける）
    }
    .to_string()
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
fn extract_account_id(token: &str) -> anyhow::Result<String> {
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
fn token_expired(token: &str) -> bool {
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
fn build_client(timeout_secs: u64) -> Client {
    Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .build()
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct ChatGptProvider {
    client: Client,
    /// Path to auth.json file (default: ~/.codex/auth.json)
    auth_file: String,
    base_url: String,
    /// OAuth トークンリフレッシュ先（テストで差し替え可能）。
    oauth_token_url: String,
    default_model: String,
    reasoning_effort: Option<String>,
    include_encrypted_content: bool,
    /// `client` に設定済みの read timeout（秒）。`Client` からは読み出せないので保持する。
    timeout_secs: u64,
    /// テレメトリ用の表示名（既定は形式名 "chatgpt"）。ルーティングキーは
    /// router 登録時に別途決まる。
    name: String,
}

impl Default for ChatGptProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatGptProvider {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            client: build_client(DEFAULT_TIMEOUT_SECS),
            auth_file: format!("{}/.codex/auth.json", home),
            base_url: CHATGPT_BASE_URL.to_string(),
            oauth_token_url: OAUTH_TOKEN_URL.to_string(),
            default_model: DEFAULT_MODEL.to_string(),
            reasoning_effort: Some("low".to_string()),
            include_encrypted_content: false,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            name: "chatgpt".to_string(),
        }
    }

    /// 表示名を上書きする（同じ形式の接続先を別名で登録するとき）。
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// チャット補完リクエストの read timeout を秒で上書きする（#433）。
    ///
    /// 既定は 60 秒。`reasoning_effort` の高い体は 1 ターンの生成がこれを超えることが
    /// あり、超えると `failed to read response body: operation timed out` になって
    /// router がリトライする。config の `[providers.chatgpt] timeout_secs` から渡す。
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.client = build_client(secs);
        self.timeout_secs = secs;
        self
    }

    /// テスト用: OAuth トークンエンドポイントを差し替える。
    pub fn with_oauth_token_url(mut self, url: impl Into<String>) -> Self {
        self.oauth_token_url = url.into();
        self
    }

    pub fn with_auth_file(mut self, path: impl Into<String>) -> Self {
        let p: String = path.into();
        self.auth_file = expand_tilde(&p);
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        let s: String = effort.into();
        self.reasoning_effort = if s.is_empty() { None } else { Some(s) };
        self
    }

    pub fn with_include_encrypted_content(mut self, v: bool) -> Self {
        self.include_encrypted_content = v;
        self
    }

    /// Read access_token from auth_file
    fn load_access_token(&self) -> Result<String> {
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
    async fn fresh_access_token(&self) -> Result<String> {
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
    async fn refresh_access_token(&self, stale_token: Option<&str>) -> Result<String> {
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

    fn request_builder(
        &self,
        endpoint: &str,
        token: &str,
        account_id: &str,
    ) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, endpoint);
        self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("chatgpt-account-id", account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "pi")
            .header("accept", "text/event-stream")
            .header("Content-Type", "application/json")
    }

    /// リクエスト中の http(s) 画像URLを取得して base64 data URI に置き換えた
    /// コピーを返す。
    ///
    /// Discord の添付画像URL（cdn.discordapp.com）は、LLM バックエンドから直接
    /// 取得しようとすると bot 判定・署名付きURLの都合で弾かれることがある。こちらで
    /// 取得して data URI として埋め込めば、外部取得に依存せず確実に読ませられる。
    /// 既に data URI のもの・非 http のものは触らない。取得失敗時は元のURLのまま
    /// 残す（送信自体は試みる）。
    async fn inline_remote_images(&self, request: &ChatRequest) -> ChatRequest {
        let mut req = request.clone();
        // 直近のユーザーターン以降の画像だけを対象にする。毎ターン全履歴を落とすと
        // 「画像数 × タイムアウト」の遅延になり、過去ターンの署名付きURLは失効して
        // いて無駄な往復になりやすいため（現在ターンの画像だけ読めれば足りる）。
        let Some(start) = req.messages.iter().rposition(|m| m.role == Role::User) else {
            return req;
        };
        for msg in req.messages[start..].iter_mut() {
            match &mut msg.content {
                Some(MessageContent::Multi(parts)) => {
                    for part in parts.iter_mut() {
                        if let ContentPart::ImageUrl { image_url } = part {
                            self.inline_one_image(image_url).await;
                        }
                    }
                }
                Some(MessageContent::Image { image_url, .. }) => {
                    self.inline_one_image(image_url).await;
                }
                _ => {}
            }
        }
        req
    }

    async fn inline_one_image(&self, image_url: &mut ImageUrl) {
        let url = image_url.url.trim();
        if url.starts_with("data:") {
            return; // 既に data URI
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return; // 相対/未知スキームは触らない
        }
        match self.download_image_data_uri(url).await {
            Ok(data_uri) => image_url.url = data_uri,
            Err(e) => {
                tracing::warn!(error = %e, "failed to inline image as data URI; keeping original url")
            }
        }
    }

    /// 画像URLをダウンロードして `data:<mime>;base64,<...>` を返す。上限 10MB。
    ///
    /// SSRF 対策: 取得前にホストを解決し、全解決 IP が公開アドレスであることを確認、
    /// その IP に固定して接続する（DNS rebinding 対策）。リダイレクトは無効化し、
    /// 内部URLへ飛ばされないようにする。画像URLはユーザー（Discord メッセージ）由来の
    /// 信頼できない入力なので、この検証を必ず通す。
    async fn download_image_data_uri(&self, url: &str) -> Result<String> {
        const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

        let parsed = reqwest::Url::parse(url).context("invalid image url")?;
        let host = parsed
            .host_str()
            .context("image url has no host")?
            .to_string();
        let pinned = validate_public_url(&parsed).await?;

        // 検証済み IP に固定・リダイレクト無効・短めのタイムアウトの専用クライアント。
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .resolve(&host, pinned)
            .build()
            .context("failed to build image http client")?;

        let resp = client
            .get(url)
            .send()
            .await
            .context("image download request failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("image download HTTP {}", resp.status());
        }
        // Content-Length があればダウンロード前に上限判定（メモリ濫用の抑制）。
        if let Some(len) = resp.content_length() {
            if len > MAX_IMAGE_BYTES {
                anyhow::bail!("image too large ({len} bytes, max 10MB)");
            }
        }
        let header_mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .filter(|c| c.starts_with("image/"));
        let bytes = resp.bytes().await.context("failed to read image body")?;
        if bytes.len() as u64 > MAX_IMAGE_BYTES {
            anyhow::bail!("image too large ({} bytes, max 10MB)", bytes.len());
        }
        let mime = header_mime.unwrap_or_else(|| guess_image_mime(url));
        Ok(format!("data:{mime};base64,{}", base64_encode(&bytes)))
    }

    /// Convert a message's content into the Responses API content value.
    ///
    /// Responses API のマルチモーダル型は `input_text` / `input_image` で、`image_url` は
    /// **文字列**（URL か data URI）である点に注意（Chat Completions の
    /// `{"type":"image_url","image_url":{"url":...}}` とは別物）。以前は Chat
    /// Completions 形式を送っており、codex/responses バックエンドでは画像が無視/拒否
    /// されていた。テキスト単体は文字列コンテンツとして送れるため従来どおり。
    fn message_content_value(content: &Option<MessageContent>) -> Option<Value> {
        match content {
            Some(MessageContent::Text(text)) => Some(serde_json::json!(text)),
            Some(MessageContent::Image { image_url, .. }) => {
                Some(serde_json::json!([Self::input_image_part(image_url),]))
            }
            Some(MessageContent::Multi(parts)) => {
                let parts_json: Vec<Value> = parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => {
                            serde_json::json!({"type": "input_text", "text": text})
                        }
                        ContentPart::ImageUrl { image_url } => Self::input_image_part(image_url),
                    })
                    .collect();
                Some(serde_json::json!(parts_json))
            }
            None => None,
        }
    }

    /// Responses API の `input_image` パートを組む。`image_url` は文字列（URL / data URI）。
    fn input_image_part(image_url: &ImageUrl) -> Value {
        let mut part = serde_json::json!({
            "type": "input_image",
            "image_url": image_url.url,
        });
        if let Some(detail) = &image_url.detail {
            part["detail"] = serde_json::json!(detail);
        }
        part
    }

    /// Build the request body in the Responses API format.
    fn build_request_body(&self, request: &ChatRequest, stream: bool) -> Value {
        let mut system_prompts: Vec<String> = Vec::new();
        tracing::warn!(
            "chatgpt build_request_body: messages count={}, system_prompts will be extracted",
            request.messages.len()
        );
        let mut input: Vec<Value> = Vec::new();

        tracing::debug!(
            message_count = request.messages.len(),
            "build_request_body: received messages"
        );
        for msg in &request.messages {
            tracing::debug!(role = ?msg.role, "build_request_body: message role");
        }

        for msg in &request.messages {
            if msg.role == Role::System {
                tracing::debug!(
                    role = "system",
                    content_is_some = msg.content.is_some(),
                    "build_request_body: processing system message"
                );
                if let Some(MessageContent::Text(text)) = &msg.content {
                    tracing::debug!(
                        text_len = text.len(),
                        "build_request_body: system message is Text, adding to system_prompts"
                    );
                    system_prompts.push(text.clone());
                } else if let Some(content) = Self::message_content_value(&msg.content) {
                    if let Some(s) = content.as_str() {
                        tracing::debug!(
                            str_len = s.len(),
                            "build_request_body: system message content converted to str via message_content_value"
                        );
                        system_prompts.push(s.to_string());
                    } else {
                        tracing::warn!(
                            content_type = ?&msg.content,
                            "build_request_body: system message content is not a string after message_content_value conversion, SKIPPING"
                        );
                    }
                } else {
                    tracing::warn!(
                        content_is_none = msg.content.is_none(),
                        "build_request_body: system message content is None or could not be converted, SKIPPING"
                    );
                }
                continue;
            }

            if msg.role == Role::Assistant {
                if let Some(tool_calls) = &msg.tool_calls {
                    if !tool_calls.is_empty() {
                        // assistant がツールコールと同時にテキストを返した場合、そのテキストも
                        // 履歴に残す（以前は continue で本文が欠落していた）。
                        // 空テキストは追加しない。
                        let has_text = msg.text_content().is_some_and(|t| !t.is_empty());
                        if has_text {
                            if let Some(content) = Self::message_content_value(&msg.content) {
                                input.push(serde_json::json!({
                                    "role": "assistant",
                                    "content": content,
                                }));
                            }
                        }
                        for tool_call in tool_calls {
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": tool_call.id,
                                "name": tool_call.function.name,
                                "arguments": tool_call.function.arguments,
                            }));
                        }
                        continue;
                    }
                    // tool_calls が空 (Some(vec![])) の場合は通常の assistant メッセージ
                    // として下の共通処理へフォールスルーする（メッセージ全体の消失を防ぐ）。
                }
            }

            if msg.role == Role::Tool {
                if let Some(tool_call_id) = &msg.tool_call_id {
                    let output = msg.text_content().unwrap_or_default();
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": tool_call_id,
                        "output": output,
                    }));
                    continue;
                }
            }

            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "user",
            };
            let mut m = serde_json::json!({"role": role});
            if let Some(content) = Self::message_content_value(&msg.content) {
                m["content"] = content;
            }
            // #892: Responses API は input item の `name` を受け付けない
            // （400 Unknown parameter: 'input[N].name'）。話者は本文へ埋め込む方針のため
            // Message.name は wire に出さない（防御）。
            input.push(m);
        }

        let mut body = serde_json::json!({
            "model": request.model,
            "store": false,
            "stream": stream,
            "input": input,
            "text": {"verbosity": "medium"},
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });

        // per-request（エージェント個別）を優先し、無ければ構築時の既定。
        if let Some(value) = request
            .reasoning_effort
            .as_deref()
            .or(self.reasoning_effort.as_deref())
        {
            body["reasoning"] = serde_json::json!({"effort": value});
        }

        // NOTE: max_output_tokens is NOT supported by the chatgpt Responses API
        // (returns 400 "Unsupported parameter: max_output_tokens") — never sent.

        if self.include_encrypted_content {
            body["include"] = serde_json::json!(["reasoning.encrypted_content"]);
        }

        tracing::warn!(
            "chatgpt build_request_body: system_prompts count={}",
            system_prompts.len()
        );
        if system_prompts.is_empty() {
            tracing::warn!(
                total_messages = request.messages.len(),
                "build_request_body: system_prompts is EMPTY! instructions field will NOT be set -> API will return 400 Bad Request"
            );
        }

        if !system_prompts.is_empty() {
            body["instructions"] = serde_json::json!(system_prompts.join("\n\n"));
        }

        let mut tools: Vec<Value> = Vec::new();
        if let Some(ref functions) = request.functions {
            tools.extend(functions.iter().map(|f| {
                serde_json::json!({
                    "type": "function",
                    "name": f.name,
                    "description": f.description,
                    "parameters": f.parameters,
                })
            }));
        }
        // 本文URL読取り（エージェント単位オプトイン）: native web_search を有効化。
        // codex CLI が同じ codex/responses バックエンドへ送るのと同じツール形
        // （external_web_access=true で live 取得、text+image 対応）。モデルが
        // search / open_page アクションでリンク先を読める。
        if request
            .metadata
            .get("web_search")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            tools.push(serde_json::json!({
                "type": "web_search",
                "external_web_access": true,
                "search_content_types": ["text", "image"],
            }));
        }
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
        }

        if let Some(ref fc) = request.function_call {
            match fc {
                FunctionCallBehavior::Mode(mode) => {
                    body["tool_choice"] = serde_json::json!(mode);
                }
                FunctionCallBehavior::Named { name } => {
                    body["tool_choice"] = serde_json::json!({"type": "function", "name": name});
                }
            }
        }

        debug!(
            model = %request.model,
            stream = stream,
            input_count = input.len(),
            system_prompt_count = system_prompts.len(),
            has_tools = request.functions.is_some(),
            body = %body,
            "chatgpt build_request_body"
        );

        body
    }

    /// Parse a fully-collected SSE response body into a `ChatResponse`.
    // #676: pub —— chatgpt の SSE パース（incomplete→Length を含む）を server 側の
    // 「incomplete→ターン失敗」end-to-end テストから直接叩けるようにする（純パーサ）。
    pub fn parse_response(&self, sse_text: &str, model: &str) -> Result<ChatResponse> {
        // #844: message アイテムごとのテキストを別々に集める。`current` が進行中アイテムの
        // 蓄積で、アイテム境界（output_item.done）で `items` に確定する。無区切り連結せず
        // アイテム境界を保持することで、下流でセンチネルが平文連結されるのを防ぐ。
        let mut items: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut id = String::new();
        let mut usage = Usage::default();
        // #676（方針3）: Responses API が出力上限で応答を打ち切ったか。status=="incomplete"
        // かつ incomplete_details.reason=="max_output_tokens" のとき真。合成する finish_reason を
        // Length に倒し、エンジンがターンを失敗させられるようにする（切り捨てを黙って最終回答に
        // しない）。chatgpt は cap 値を送らないので、これがモデル内部既定に当たった gpt-5.6 等を
        // 守る実質的な防衛線になる。
        let mut truncated_by_max_tokens = false;
        let mut dbg_data_line_count: usize = 0;
        let mut dbg_delta_event_count: usize = 0;
        let mut current_event = String::new();

        for line in sse_text.lines() {
            let line = line.trim();
            if let Some(ev) = line.strip_prefix("event:") {
                current_event = ev.trim().to_string();
                continue;
            }
            let data = match line.strip_prefix("data:") {
                Some(d) => d.trim(),
                None => continue,
            };
            dbg_data_line_count += 1;
            if data == "[DONE]" {
                current_event.clear();
                continue;
            }
            let parsed: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => {
                    current_event.clear();
                    continue;
                }
            };
            // Effective event type: prefer parsed["type"], fall back to current_event.
            let effective_event = parsed["type"].as_str().unwrap_or(&current_event);
            match effective_event {
                "response.output_text.delta" => {
                    if let Some(delta) = parsed["delta"].as_str() {
                        dbg_delta_event_count += 1;
                        current.push_str(delta);
                    }
                }
                "response.output_item.done" | "response.output_item.completed" => {
                    if let Some(call) = Self::parse_function_call_item(&parsed["item"]) {
                        tool_calls.push(call);
                    }
                    // #844: message アイテムの境界。蓄積テキストがあれば 1 アイテムとして
                    // 確定し、次アイテムのテキストと無区切り連結されるのを断ち切る。
                    // function_call / reasoning アイテムはテキストを積まないので current は空。
                    if !current.is_empty() {
                        items.push(std::mem::take(&mut current));
                    }
                }
                "response.completed" | "response.done" | "response.incomplete" => {
                    if let Some(rid) = parsed["response"]["id"].as_str() {
                        id = rid.to_string();
                    }
                    // #676（方針3）: 出力上限による打ち切りを拾う。incomplete イベントだけでなく、
                    // completed に status/incomplete_details が載る実装差にも耐えるよう、イベント
                    // 名でなく response 本体の status/reason で判定する。
                    if parsed["response"]["status"].as_str() == Some("incomplete")
                        && parsed["response"]["incomplete_details"]["reason"].as_str()
                            == Some("max_output_tokens")
                    {
                        truncated_by_max_tokens = true;
                    }
                    if let Some(output) = parsed["response"]["output"].as_array() {
                        for item in output {
                            if let Some(call) = Self::parse_function_call_item(item) {
                                if !tool_calls.iter().any(|tc| tc.id == call.id) {
                                    tool_calls.push(call);
                                }
                            }
                        }
                    }
                    let u = &parsed["response"]["usage"];
                    // Responses API は cached 分を usage.input_tokens_details.cached_tokens
                    // にネストして返す（codex CLI が flat な cached_input_tokens で返すのとは
                    // 構造が違う点に注意）。フィールドが無い/ null のときは 0 に倒す。
                    let cached = u["input_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0) as u32;
                    usage = Usage {
                        prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0) as u32,
                        completion_tokens: u["output_tokens"].as_u64().unwrap_or(0) as u32,
                        total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
                        cache_read_input_tokens: cached,
                        // OpenAI はキャッシュ書き込みを別課金しない（cache write は
                        // 通常の input と同じ料金）ため、Anthropic のような
                        // cache_creation の概念が無く、Responses API も該当フィールドを
                        // 返さない。ここが 0 なのはバグではなく仕様。
                        cache_creation_input_tokens: 0,
                    };
                }
                "error" => {
                    let msg = parsed["message"]
                        .as_str()
                        .or_else(|| parsed["error"]["message"].as_str())
                        .unwrap_or("unknown error");
                    anyhow::bail!("ChatGPT API error: {}", msg);
                }
                _ => {}
            }
            current_event.clear();
        }

        // #844: output_item.done が来ないまま終わる系（既存テストの delta のみ SSE 等）の
        // 取りこぼしを防ぐ最終フラッシュ。1 アイテムだけの通常応答はここで確定する。
        if !current.is_empty() {
            items.push(std::mem::take(&mut current));
        }

        // #844: アイテム毎に NO_REPLY 判定し、非センチネル（trim 後 NO_REPLY 全文一致でない）
        // かつ非空の先頭アイテムを採用する（実測で first が正解）。実体のあるアイテムが
        // 無い場合、いずれかがセンチネルなら NO_REPLY を残し（下流の沈黙判定を活かす）、
        // そうでなければ空応答（None）にする。単一アイテムの通常応答は従来と同じ結果になる。
        let selected: Option<String> = match items
            .iter()
            .find(|t| {
                let s = t.trim();
                !s.is_empty() && s != NO_REPLY_SENTINEL
            })
            .cloned()
        {
            Some(text) => Some(text),
            None if items.iter().any(|t| t.trim() == NO_REPLY_SENTINEL) => {
                Some(NO_REPLY_SENTINEL.to_string())
            }
            None => None,
        };

        tracing::warn!(
            "chatgpt parse_response: data_lines={} delta_events={} message_items={} selected_bytes={} tool_calls={}",
            dbg_data_line_count,
            dbg_delta_event_count,
            items.len(),
            selected.as_deref().map(str::len).unwrap_or(0),
            tool_calls.len(),
        );

        let content = selected.map(MessageContent::Text);
        // #676（方針3）: 出力上限による打ち切りは tool_calls / content の有無より優先して
        // Length にする。切り捨てられた応答は tool_call JSON も本文も途中で切れており、最終回答
        // にもツール往復の一手にもしてはならない（エンジンがこの Length を見てターンを失敗させる）。
        let finish_reason = if truncated_by_max_tokens {
            FinishReason::Length
        } else if tool_calls.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        };
        let tool_calls = if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        };

        Ok(ChatResponse {
            id,
            model: model.to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content,
                    name: None,
                    function_call: None,
                    tool_calls,
                    tool_call_id: None,
                },
                finish_reason: Some(finish_reason),
            }],
            usage,
            created: 0,
        })
    }

    fn parse_function_call_item(item: &Value) -> Option<ToolCall> {
        if item["type"].as_str()? != "function_call" {
            return None;
        }

        let name = item["name"].as_str()?.to_string();
        let arguments = match item.get("arguments") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            Some(Value::Object(_)) | Some(Value::Array(_)) => item["arguments"].to_string(),
            _ => "{}".to_string(),
        };
        let id = item["call_id"]
            .as_str()
            .or_else(|| item["id"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        Some(ToolCall {
            id,
            call_type: "function".to_string(),
            function: FunctionCall { name, arguments },
        })
    }
}

#[async_trait]
impl LlmProvider for ChatGptProvider {
    fn name(&self) -> &str {
        &self.name
    }

    // #676: Responses API は max_output_tokens が 400（Unsupported parameter）なので
    // 送らない（build_request_body でも載せない）。よって出力上限のモデル登録は不要
    // （opt-out）。切り捨て検知は方針3の incomplete_details→Length→bail が担う。
    fn sends_max_output_tokens(&self) -> bool {
        false
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(vec![
            // GPT-5.6 系（gpt-5.6 は Sol にエイリアス）。codex CLI と同じ
            // codex/responses バックエンドを叩くため、同じサブスクで利用できる。
            // codex サブプロセスと違い画像（image_url）とネイティブ function
            // calling に対応するので、画像を読ませたいエージェントはこちらを使う。
            ModelInfo {
                id: "gpt-5.6".to_string(),
                name: "GPT-5.6 (Sol)".to_string(),
                context_window: 400_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-5.6-sol".to_string(),
                name: "GPT-5.6 Sol".to_string(),
                context_window: 400_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-5.6-terra".to_string(),
                name: "GPT-5.6 Terra".to_string(),
                context_window: 400_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-5.6-luna".to_string(),
                name: "GPT-5.6 Luna".to_string(),
                context_window: 400_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-5.5".to_string(),
                name: "GPT-5.5".to_string(),
                context_window: 128000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-4o".to_string(),
                name: "GPT-4o".to_string(),
                context_window: 128000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-4.5-preview".to_string(),
                name: "GPT-4.5 Preview".to_string(),
                context_window: 128000,
                supports_function_calling: true,
                supports_vision: true,
            },
        ])
    }

    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        debug!(model = %request.model, "ChatGPT chat completion");
        let mut token = self.fresh_access_token().await?;
        let mut account_id = extract_account_id(&token)?;
        // http(s) 画像は自分で取得して data URI 化してから送る（後述）。
        let request = self.inline_remote_images(&request).await;
        let body = self.build_request_body(&request, true);
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        tracing::warn!(
            model = %request.model,
            has_instructions = body.get("instructions").is_some(),
            instructions_len = body["instructions"].as_str().map(|s| s.len()).unwrap_or(0),
            input_count = body["input"].as_array().map(|a| a.len()).unwrap_or(0),
            has_reasoning = body.get("reasoning").is_some(),
            reasoning_effort = body["reasoning"]["effort"].as_str().unwrap_or("none"),
            body_len = body_str.len(),
            "ChatGPT chat_completion: sending request"
        );
        // リトライは router が所有する（#46）。以前はここで独自に3回リトライしており、
        // router の同一プロバイダ3回リトライと重なって最大9回の HTTP 試行になっていた。
        // 429/5xx は型付き api_error で返せば router が retryable と分類して再試行する。
        // 例外: 401 だけはトークンリフレッシュがプロバイダの能力なので、ここで
        // 1回だけリフレッシュ→再送する（router は auth エラーをリトライしない）。
        let mut refreshed = false;
        let (status, text) = loop {
            let resp = self
                .request_builder("codex/responses", &token, &account_id)
                .json(&body)
                .send()
                .await
                .context("ChatGPT API request failed")?;

            let status = resp.status();
            let text = resp
                .text()
                .await
                .context("ChatGPT: failed to read response body")?;

            tracing::warn!(status = %status, body_len = text.len(), "ChatGPT chat_completion response received");

            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                tracing::info!("ChatGPT returned 401; refreshing token and retrying once");
                token = self.refresh_access_token(Some(&token)).await?;
                account_id = extract_account_id(&token)?;
                continue;
            }
            break (status, text);
        };

        if !status.is_success() {
            tracing::warn!(status = %status, body = %text, "ChatGPT chat_completion error response");
            return Err(crate::error::api_error("ChatGPT", status, text));
        }

        let result = self.parse_response(&text, &request.model);
        tracing::warn!(
            success = result.is_ok(),
            "ChatGPT chat_completion parse result"
        );
        result
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "ChatGPT streaming chat completion");
        let mut token = self.fresh_access_token().await?;
        let mut account_id = extract_account_id(&token)?;
        let request = self.inline_remote_images(&request).await;
        let body = self.build_request_body(&request, true);
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        tracing::warn!(
            model = %request.model,
            has_instructions = body.get("instructions").is_some(),
            instructions_len = body["instructions"].as_str().map(|s| s.len()).unwrap_or(0),
            input_count = body["input"].as_array().map(|a| a.len()).unwrap_or(0),
            has_reasoning = body.get("reasoning").is_some(),
            reasoning_effort = body["reasoning"]["effort"].as_str().unwrap_or("none"),
            body_len = body_str.len(),
            "ChatGPT chat_completion_stream: sending request"
        );
        // リトライは router が所有する（#46: 内部リトライとの重なりで最大9試行に
        // なっていた）。エラーは型付き api_error で返し、router の分類に委ねる。
        // 例外: 401 のみプロバイダ責務としてリフレッシュ→1回だけ再送（非ストリーム側と同じ）。
        let mut refreshed = false;
        let resp = loop {
            let resp = self
                .request_builder("codex/responses", &token, &account_id)
                .json(&body)
                .send()
                .await
                .context("ChatGPT streaming request failed")?;

            let status = resp.status();
            tracing::warn!(status = %status, "ChatGPT chat_completion_stream response received");
            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                tracing::info!("ChatGPT returned 401 (stream); refreshing token and retrying once");
                token = self.refresh_access_token(Some(&token)).await?;
                account_id = extract_account_id(&token)?;
                continue;
            }
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                tracing::warn!(status = %status, body = %text, "ChatGPT chat_completion_stream error response");
                return Err(crate::error::api_error("ChatGPT", status, text));
            }
            break resp;
        };
        let request_model = request.model.clone();
        // チャンク境界を跨いでバッファし、SSEの `data:` 行ごとに1デルタを emit する。
        // Responses API は data ペイロード自身に `type` を含むため、行を跨ぐ `event:` 状態には
        // 依存しない。これにより、同一チャンク内で後続イベントが直前のテキストデルタを
        // 上書きしてしまう問題を防ぐ。
        let stream =
            crate::providers::sse::line_stream(resp.bytes_stream()).filter_map(move |line_res| {
                let request_model = request_model.clone();
                let out = match line_res {
                    Err(e) => Some(Err(e)),
                    Ok(line) => {
                        let line = line.trim();
                        match line.strip_prefix("data:").map(|d| d.trim()) {
                            None => None,
                            Some("[DONE]") => None,
                            Some(data) => match serde_json::from_str::<Value>(data) {
                                Err(_) => None,
                                Ok(parsed) => match parsed["type"].as_str().unwrap_or_default() {
                                    "response.output_text.delta" => {
                                        let delta_text = parsed["delta"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_string();
                                        Some(Ok(ChatStreamDelta {
                                            id: String::new(),
                                            model: request_model,
                                            choices: vec![StreamChoice {
                                                index: 0,
                                                delta: DeltaMessage {
                                                    role: None,
                                                    content: Some(delta_text),
                                                    function_call: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: None,
                                            }],
                                        }))
                                    }
                                    "response.completed" | "response.done" => {
                                        // Tool calls are ignored for now (future work).
                                        Some(Ok(ChatStreamDelta {
                                            id: String::new(),
                                            model: request_model,
                                            choices: vec![StreamChoice {
                                                index: 0,
                                                delta: DeltaMessage {
                                                    role: None,
                                                    content: Some(String::new()),
                                                    function_call: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: Some(FinishReason::Stop),
                                            }],
                                        }))
                                    }
                                    _ => None,
                                },
                            },
                        }
                    }
                };
                futures::future::ready(out)
            });
        Ok(Box::pin(stream))
    }

    fn supports_function_calling(&self) -> bool {
        true
    }

    fn supports_vision(&self) -> bool {
        true
    }

    async fn health_check(&self) -> Result<bool> {
        Ok(self.load_access_token().is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// マルチモーダル（画像）ユーザーメッセージが Responses API 形式
    /// （input_text / input_image・image_url は文字列）で組まれること。
    /// 以前は Chat Completions 形式（type:image_url, image_url:{url}）で、
    /// codex/responses バックエンドでは画像が無視/拒否されていた。
    #[test]
    fn test_message_content_value_multimodal_uses_responses_format() {
        let content = Some(MessageContent::Multi(vec![
            ContentPart::Text {
                text: "この画像を見て".to_string(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "https://cdn.discordapp.com/x.png".to_string(),
                    detail: Some("auto".to_string()),
                },
            },
        ]));
        let v = ChatGptProvider::message_content_value(&content).unwrap();
        let arr = v.as_array().expect("content is an array");
        assert_eq!(arr[0]["type"], "input_text");
        assert_eq!(arr[0]["text"], "この画像を見て");
        assert_eq!(arr[1]["type"], "input_image");
        // image_url は文字列（オブジェクトではない）。
        assert_eq!(arr[1]["image_url"], "https://cdn.discordapp.com/x.png");
        assert_eq!(arr[1]["detail"], "auto");
    }

    /// RFC 4648 のテストベクタで base64 符号化を検証（壊れると画像が壊れる）。
    #[test]
    fn test_base64_encode_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        // 非 ASCII バイト（0xFF 等）も正しく符号化。
        assert_eq!(base64_encode(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(base64_encode(&[0x00]), "AA==");
    }

    #[test]
    fn test_is_global_ip_rejects_internal() {
        use std::net::IpAddr;
        let bad = [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254", // クラウドメタデータ
            "100.64.0.1",      // CGNAT
            "0.0.0.0",
            "::1",
            "::ffff:127.0.0.1", // v4-mapped loopback
            "::ffff:169.254.169.254",
            "fe80::1",
            "fc00::1",
            "::7f00:1",           // ::127.0.0.1 (IPv4-compatible, deprecated)
            "2002:7f00:1::",      // 6to4 embedding 127.0.0.1
            "2001:0:0:0:0:0:0:1", // Teredo 2001:0000::/32
            "64:ff9b::7f00:1",    // NAT64 embedding 127.0.0.1
        ];
        for s in bad {
            let ip: IpAddr = s.parse().unwrap();
            assert!(!is_global_ip(ip), "{s} should be rejected");
        }
        let good = ["8.8.8.8", "1.1.1.1", "2001:4860:4860::8888"];
        for s in good {
            let ip: IpAddr = s.parse().unwrap();
            assert!(is_global_ip(ip), "{s} should be allowed");
        }
    }

    #[tokio::test]
    async fn test_validate_public_url_rejects_localhost_and_scheme() {
        // 明示 IP のループバックは解決不要で弾ける。
        let u = reqwest::Url::parse("http://127.0.0.1:8080/x.png").unwrap();
        assert!(validate_public_url(&u).await.is_err());
        // 非 http スキーム。
        let u = reqwest::Url::parse("ftp://example.com/x.png").unwrap();
        assert!(validate_public_url(&u).await.is_err());
    }

    #[test]
    fn test_guess_image_mime() {
        assert_eq!(guess_image_mime("https://x/y.png"), "image/png");
        assert_eq!(guess_image_mime("https://x/y.JPG?ex=1&is=2"), "image/jpeg");
        assert_eq!(guess_image_mime("https://x/y.webp"), "image/webp");
        assert_eq!(guess_image_mime("https://x/y.gif#frag"), "image/gif");
        assert_eq!(guess_image_mime("https://x/noext"), "image/png");
    }

    /// data: URI と非 http はダウンロードを試みず素通しすること（ネットワーク不要）。
    #[tokio::test]
    async fn test_inline_one_image_skips_data_and_non_http() {
        let p = ChatGptProvider::new();
        let mut data = ImageUrl {
            url: "data:image/png;base64,AAAA".to_string(),
            detail: None,
        };
        p.inline_one_image(&mut data).await;
        assert_eq!(data.url, "data:image/png;base64,AAAA");

        let mut rel = ImageUrl {
            url: "file/local.png".to_string(),
            detail: None,
        };
        p.inline_one_image(&mut rel).await;
        assert_eq!(rel.url, "file/local.png");
    }

    /// Model used by the real-API (`--ignored`) tests. ChatGPT/Codex accounts
    /// reject `gpt-4o`, so we use the provider default path (`gpt-5.5`).
    const TEST_MODEL: &str = DEFAULT_MODEL;

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

    #[test]
    fn test_build_request_body_max_output_tokens() {
        // max_output_tokens must NOT appear in the request body (unsupported by the API).
        let provider = ChatGptProvider::new();
        let mut request = ChatRequest::new("gpt-5.5", vec![Message::user("hi")]);
        request.max_tokens = Some(256);
        let body = provider.build_request_body(&request, false);
        assert!(
            body.get("max_output_tokens").is_none(),
            "max_output_tokens must not be sent to the API"
        );

        let request_none = ChatRequest::new("gpt-5.5", vec![Message::user("hi")]);
        let body_none = provider.build_request_body(&request_none, false);
        assert!(body_none.get("max_output_tokens").is_none());
    }

    /// #884 PR2 並列呼び出し: 同一生成の並列 ToolCall（1 assistant に tool_calls×2）は
    /// Responses(chatgpt) wire で `function_call`×2 の input アイテムに展開し、対応する
    /// 連続 Role::Tool は `function_call_output`×2 に展開する（call_id で 1:1 対応）。
    /// core の集約 (`assemble_parallel_calls_grouped_into_one_assistant`) と対になる
    /// provider snapshot。
    #[test]
    fn parallel_tool_calls_expand_to_two_function_calls_and_two_outputs() {
        let provider = ChatGptProvider::new();
        let mut assistant = Message::assistant("");
        assistant.content = None;
        assistant.tool_calls = Some(vec![
            ToolCall {
                id: "call_a".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "execute_shell".to_string(),
                    arguments: r#"{"command":"echo","args":["a"]}"#.to_string(),
                },
            },
            ToolCall {
                id: "call_b".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "execute_shell".to_string(),
                    arguments: r#"{"command":"echo","args":["b"]}"#.to_string(),
                },
            },
        ]);
        let request = ChatRequest::new(
            "gpt-5.5",
            vec![
                Message::system("sys"),
                Message::user("並列で a と b を実行して"),
                assistant,
                Message::tool("call_a", r#"{"exit_code":0,"stdout":"a"}"#),
                Message::tool("call_b", r#"{"exit_code":0,"stdout":"b"}"#),
            ],
        );

        let body = provider.build_request_body(&request, false);
        let input = body["input"].as_array().expect("input must be an array");

        // function_call が 2 個・function_call_output が 2 個そろう。
        let function_calls: Vec<&Value> = input
            .iter()
            .filter(|item| item["type"] == "function_call")
            .collect();
        let function_outputs: Vec<&Value> = input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .collect();
        assert_eq!(
            function_calls.len(),
            2,
            "並列 2 呼び出しが function_call×2 に展開"
        );
        assert_eq!(
            function_outputs.len(),
            2,
            "並列 2 結果が function_call_output×2 に展開"
        );

        // call_id が 1:1 対応で保存される。
        assert_eq!(function_calls[0]["call_id"], serde_json::json!("call_a"));
        assert_eq!(function_calls[1]["call_id"], serde_json::json!("call_b"));
        assert_eq!(function_outputs[0]["call_id"], serde_json::json!("call_a"));
        assert_eq!(function_outputs[1]["call_id"], serde_json::json!("call_b"));

        // 順序: 2 つの function_call が両 output より前に並ぶ（生成→結果の順）。
        let first_output_pos = input
            .iter()
            .position(|item| item["type"] == "function_call_output")
            .expect("function_call_output must exist");
        let last_call_pos = input
            .iter()
            .rposition(|item| item["type"] == "function_call")
            .expect("function_call must exist");
        assert!(
            last_call_pos < first_output_pos,
            "function_call は function_call_output より前"
        );
    }

    #[test]
    fn test_reasoning_effort_never_emits_max_output_tokens() {
        // max_output_tokens は Responses API 非対応のため、いかなる設定でも body に現れない。
        let low = ChatGptProvider::new().with_reasoning_effort("low");
        let body_low = low.build_request_body(
            &ChatRequest::new("gpt-5.5", vec![Message::user("hi")]),
            false,
        );
        assert!(
            body_low.get("max_output_tokens").is_none(),
            "max_output_tokens must not be sent to the API"
        );

        let high = ChatGptProvider::new().with_reasoning_effort("high");
        let body_high = high.build_request_body(
            &ChatRequest::new("gpt-5.5", vec![Message::user("hi")]),
            false,
        );
        assert!(body_high.get("max_output_tokens").is_none());
    }

    /// metadata の web_search=true で native web_search ツールが tools に載ること
    /// （codex CLI が同じバックエンドへ送るのと同じ形）。未設定なら載らない。
    #[test]
    fn test_build_request_body_web_search_tool() {
        let provider = ChatGptProvider::new();

        // 有効時: function ツールと併存して web_search が入る。
        let mut request = ChatRequest::new("gpt-5.6-sol", vec![Message::user("このURLを見て")]);
        request
            .metadata
            .insert("web_search".to_string(), serde_json::json!(true));
        request.functions = Some(vec![FunctionDefinition {
            name: "my_tool".to_string(),
            description: None,
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]);
        let body = provider.build_request_body(&request, false);
        let tools = body["tools"].as_array().expect("tools array");
        assert!(tools.iter().any(|t| t["type"] == "function"));
        let ws = tools
            .iter()
            .find(|t| t["type"] == "web_search")
            .expect("web_search tool present");
        assert_eq!(ws["external_web_access"], true);
        assert_eq!(
            ws["search_content_types"],
            serde_json::json!(["text", "image"])
        );

        // 未設定時: web_search は載らない（functions 無しなら tools 自体無し）。
        let plain = ChatRequest::new("gpt-5.6-sol", vec![Message::user("hi")]);
        let body = provider.build_request_body(&plain, false);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn test_build_request_body_converts_assistant_tool_calls_to_function_call_items() {
        let provider = ChatGptProvider::new();
        let mut assistant = Message::assistant("");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "get_weather".to_string(),
                arguments: r#"{"city":"Tokyo"}"#.to_string(),
            },
        }]);
        let request =
            ChatRequest::new("gpt-5.5", vec![Message::user("hi"), assistant]).with_max_tokens(256);

        let body = provider.build_request_body(&request, false);

        assert!(body.get("max_output_tokens").is_none());
        let input = body["input"].as_array().expect("input must be an array");
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["type"], serde_json::json!("function_call"));
        assert_eq!(input[1]["call_id"], serde_json::json!("call_1"));
        assert_eq!(input[1]["name"], serde_json::json!("get_weather"));
        assert_eq!(
            input[1]["arguments"],
            serde_json::json!(r#"{"city":"Tokyo"}"#)
        );
        assert!(
            input[1].get("role").is_none(),
            "function_call input items must not be role messages"
        );
        assert!(
            input[1].get("content").is_none(),
            "function_call input items must not require content"
        );

        fn contains_key(value: &Value, key: &str) -> bool {
            match value {
                Value::Object(map) => {
                    map.contains_key(key) || map.values().any(|v| contains_key(v, key))
                }
                Value::Array(values) => values.iter().any(|v| contains_key(v, key)),
                _ => false,
            }
        }
        assert!(
            !contains_key(&body, "tool_calls"),
            "tool_calls must not be sent to the Responses API"
        );
        assert!(
            !contains_key(&body, "tool_call_id"),
            "tool_call_id must not be sent to the Responses API"
        );
    }

    #[test]
    fn test_build_request_body_keeps_assistant_text_alongside_tool_calls() {
        let provider = ChatGptProvider::new();
        let mut assistant = Message::assistant("I'll check the weather");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "get_weather".to_string(),
                arguments: r#"{"city":"Tokyo"}"#.to_string(),
            },
        }]);
        let request = ChatRequest::new("gpt-5.5", vec![Message::user("hi"), assistant]);

        let body = provider.build_request_body(&request, false);
        let input = body["input"].as_array().expect("input must be an array");

        // user, assistant-text, function_call の3要素になる。
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["role"], serde_json::json!("assistant"));
        assert!(input[1]["content"]
            .to_string()
            .contains("I'll check the weather"));
        assert_eq!(input[2]["type"], serde_json::json!("function_call"));
    }

    #[test]
    fn test_build_request_body_converts_tool_result_to_function_call_output_item() {
        let provider = ChatGptProvider::new();
        let mut assistant = Message::assistant("");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "get_weather".to_string(),
                arguments: r#"{"city":"Tokyo"}"#.to_string(),
            },
        }]);
        let tool = Message::tool("call_1", r#"{"temperature":22}"#);
        let request = ChatRequest::new("gpt-5.5", vec![Message::user("hi"), assistant, tool]);

        let body = provider.build_request_body(&request, false);

        let input = body["input"].as_array().expect("input must be an array");
        assert_eq!(input.len(), 3);
        assert_eq!(input[1]["type"], serde_json::json!("function_call"));
        assert_eq!(input[2]["type"], serde_json::json!("function_call_output"));
        assert_eq!(input[2]["call_id"], serde_json::json!("call_1"));
        assert_eq!(
            input[2]["output"],
            serde_json::json!(r#"{"temperature":22}"#)
        );
        assert!(
            input[2].get("role").is_none(),
            "function_call_output input items must not be role messages"
        );
        assert!(
            input[2].get("content").is_none(),
            "function_call_output input items must not use message content"
        );
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn test_typed_history_past_turn_converts_to_function_call_items() {
        let provider = ChatGptProvider::new();
        let mut assistant = Message::assistant("");
        assistant.content = None;
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_c253".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "execute_shell".to_string(),
                arguments: r#"{"args":["60"],"command":"sleep","stdin":"","timeout_secs":90}"#
                    .to_string(),
            },
        }]);
        let request = ChatRequest::new(
            "gpt-5.5",
            vec![
                Message::system("sys"),
                Message::user("くらぶ、60秒sleepして終わったら教えて"),
                assistant,
                Message::tool(
                    "call_c253",
                    r#"{"status":"completed","exit_code":0,"stdout":""}"#,
                ),
                Message::user("終わった？"),
            ],
        );

        let body = provider.build_request_body(&request, false);
        let input = body["input"].as_array().expect("input must be an array");

        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["role"], serde_json::json!("user"));
        assert!(input[0]["content"]
            .as_str()
            .is_some_and(|text| text.contains("くらぶ、60秒sleepして終わったら教えて")));

        assert_eq!(input[1]["type"], serde_json::json!("function_call"));
        assert_eq!(input[1]["call_id"], serde_json::json!("call_c253"));
        assert_eq!(input[1]["name"], serde_json::json!("execute_shell"));
        let arguments = input[1]["arguments"]
            .as_str()
            .expect("function_call arguments must be a string");
        assert!(arguments.contains("sleep"));
        assert!(arguments.contains("60"));

        assert_eq!(input[2]["type"], serde_json::json!("function_call_output"));
        assert_eq!(input[2]["call_id"], serde_json::json!("call_c253"));
        assert!(input[2]["output"]
            .as_str()
            .is_some_and(|output| output.contains("exit_code")));
        assert!(
            input[2].get("role").is_none(),
            "function_call_output input items must not be role messages"
        );

        assert_eq!(input[3]["role"], serde_json::json!("user"));
        assert_eq!(input[3]["content"], serde_json::json!("終わった？"));

        fn contains_log_marker(value: &Value) -> bool {
            match value {
                Value::String(text) => text.contains("→log:"),
                Value::Array(values) => values.iter().any(contains_log_marker),
                Value::Object(map) => map.values().any(contains_log_marker),
                _ => false,
            }
        }
        assert!(!input.iter().any(contains_log_marker));
    }

    #[test]
    fn test_responses_input_drops_message_name() {
        // #892: Responses API は input item の `name` を拒否する
        // （400 Unknown parameter: 'input[N].name'）。Message.name が設定されていても
        // wire の input item には name キーを出さない（防御）。
        let provider = ChatGptProvider::new();
        let mut user = Message::user("終わった？");
        user.name = Some("owner".to_string());
        let request = ChatRequest::new("gpt-5.5", vec![Message::system("sys"), user]);

        let body = provider.build_request_body(&request, false);
        let input = body["input"].as_array().expect("input must be an array");

        assert_eq!(input.len(), 1, "system は input から除かれる");
        assert_eq!(input[0]["role"], serde_json::json!("user"));
        assert!(
            input[0].get("name").is_none(),
            "input item に name キーを出さない: {}",
            input[0]
        );
    }

    #[test]
    fn test_parse_response_tool_calls_from_completed_output() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",",
            "\"output\":[{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",",
            "\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Tokyo\\\"}\"}],",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.5")
            .expect("parse failed");
        assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::ToolCalls));
        let calls = resp.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("tool calls must be parsed");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].call_type, "function");
        assert_eq!(calls[0].function.name, "get_weather");
        assert_eq!(calls[0].function.arguments, r#"{"city":"Tokyo"}"#);
        assert_eq!(resp.usage.completion_tokens, 5);
        assert!(resp.choices[0].message.content.is_none());
    }

    #[test]
    fn test_parse_response_tool_calls_from_output_item_done() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",",
            "\"id\":\"fc_2\",\"call_id\":\"call_2\",\"name\":\"search\",",
            "\"arguments\":\"{\\\"query\\\":\\\"opencrab\\\"}\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-2\",",
            "\"output\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":4,\"total_tokens\":11}}}\n",
            "\n",
        );

        let resp = provider
            .parse_response(sse, "gpt-5.5")
            .expect("parse failed");
        assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::ToolCalls));
        let calls = resp.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("tool calls must be parsed");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_2");
        assert_eq!(calls[0].function.name, "search");
        assert_eq!(calls[0].function.arguments, r#"{"query":"opencrab"}"#);
    }

    /// #502: cached_tokens が usage.input_tokens_details.cached_tokens にあるとき
    /// cache_read_input_tokens に反映されること。
    #[test]
    fn test_parse_response_reads_cached_tokens() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-c\",",
            "\"output\":[],\"usage\":{\"input_tokens\":100,\"output_tokens\":20,",
            "\"total_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":80}}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.5")
            .expect("parse failed");
        assert_eq!(resp.usage.prompt_tokens, 100);
        assert_eq!(resp.usage.completion_tokens, 20);
        assert_eq!(resp.usage.cache_read_input_tokens, 80);
        // OpenAI はキャッシュ書き込みを別課金しないため常に 0。
        assert_eq!(resp.usage.cache_creation_input_tokens, 0);
    }

    /// #676（方針3）: Responses API が出力上限で応答を打ち切ったとき（status=="incomplete"
    /// かつ incomplete_details.reason=="max_output_tokens"）、finish_reason=Length にする。
    /// 前置きテキストが出ていても Stop に倒さない（エンジンがこの Length を見て切り捨てを
    /// 失敗させる。chatgpt は cap を送らないので、これが in-use の gpt-5.6 系を守る防衛線）。
    #[test]
    fn test_parse_response_incomplete_max_output_tokens_is_length() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"これから報告を書\"}\n",
            "\n",
            "event: response.incomplete\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp-inc\",",
            "\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},",
            "\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":4096,",
            "\"total_tokens\":4106}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.6-sol")
            .expect("parse failed");
        assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::Length));
        assert_eq!(resp.usage.completion_tokens, 4096);
    }

    /// #676 回帰防止: status=="completed"（打ち切りなし）は従来どおり Stop のまま。
    #[test]
    fn test_parse_response_completed_status_stays_stop() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"完了\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-ok\",",
            "\"status\":\"completed\",\"output\":[],",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"total_tokens\":12}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.6-sol")
            .expect("parse failed");
        assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::Stop));
    }

    /// #502: input_tokens_details / cached_tokens が欠落しても panic せず 0 に倒れること。
    #[test]
    fn test_parse_response_missing_cached_tokens_defaults_to_zero() {
        let provider = ChatGptProvider::new();
        // details ごと欠落。
        let sse_missing = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-m\",",
            "\"output\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":4,\"total_tokens\":11}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse_missing, "gpt-5.5")
            .expect("parse failed");
        assert_eq!(resp.usage.prompt_tokens, 7);
        assert_eq!(resp.usage.cache_read_input_tokens, 0);

        // cached_tokens が null のケース。
        let sse_null = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-n\",",
            "\"output\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":4,\"total_tokens\":11,",
            "\"input_tokens_details\":{\"cached_tokens\":null}}}}\n",
            "\n",
        );
        let resp_null = provider
            .parse_response(sse_null, "gpt-5.5")
            .expect("parse failed");
        assert_eq!(resp_null.usage.cache_read_input_tokens, 0);
    }

    // ── build_request_body field validation ──────────────────────────────────

    #[test]
    fn test_request_body_required_fields() {
        let provider = ChatGptProvider::new();
        let request = ChatRequest::new(
            "gpt-5.5",
            vec![Message::system("You are helpful."), Message::user("Hello")],
        );
        let body = provider.build_request_body(&request, false);

        // Required fields must be present.
        assert_eq!(body["model"], serde_json::json!("gpt-5.5"));
        assert!(body.get("input").is_some(), "input field must be present");
        assert_eq!(body["stream"], serde_json::json!(false));
        assert_eq!(body["store"], serde_json::json!(false));

        // max_output_tokens must NEVER appear (unsupported by Responses API).
        assert!(
            body.get("max_output_tokens").is_none(),
            "max_output_tokens must not be sent to the API"
        );
    }

    #[test]
    fn test_request_body_stream_flag() {
        let provider = ChatGptProvider::new();
        let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
        let body_stream = provider.build_request_body(&request, true);
        assert_eq!(body_stream["stream"], serde_json::json!(true));
        let body_no_stream = provider.build_request_body(&request, false);
        assert_eq!(body_no_stream["stream"], serde_json::json!(false));
    }

    #[test]
    fn test_request_body_reasoning_effort_low() {
        let provider = ChatGptProvider::new().with_reasoning_effort("low");
        let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
        let body = provider.build_request_body(&request, false);
        assert_eq!(
            body["reasoning"]["effort"],
            serde_json::json!("low"),
            "reasoning.effort must be 'low'"
        );
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn test_request_body_reasoning_effort_high() {
        let provider = ChatGptProvider::new().with_reasoning_effort("high");
        let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
        let body = provider.build_request_body(&request, false);
        assert_eq!(
            body["reasoning"]["effort"],
            serde_json::json!("high"),
            "reasoning.effort must be 'high'"
        );
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn test_request_body_no_reasoning_by_default() {
        // Default new() sets reasoning_effort = Some("low"), so "reasoning" WILL appear.
        // But if we explicitly clear it, it must not appear.
        let mut provider = ChatGptProvider::new();
        provider.reasoning_effort = None;
        let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
        let body = provider.build_request_body(&request, false);
        assert!(
            body.get("reasoning").is_none(),
            "reasoning field must not appear when reasoning_effort is None"
        );
    }

    #[test]
    fn test_request_body_instructions_from_system_message() {
        let provider = ChatGptProvider::new();
        let request = ChatRequest::new(
            "gpt-5.5",
            vec![Message::system("Be concise."), Message::user("Hi")],
        );
        let body = provider.build_request_body(&request, false);
        let instructions = body["instructions"]
            .as_str()
            .expect("instructions must be set");
        assert!(
            instructions.contains("Be concise."),
            "instructions must include system message"
        );
    }

    // ── parse_response delta text extraction ─────────────────────────────────

    #[test]
    fn test_parse_response_text_delta_single() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello, world!\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"output\":[],",
            "\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"total_tokens\":8}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.5")
            .expect("parse failed");
        let text = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.as_str(),
            other => panic!("expected Text content, got: {other:?}"),
        };
        assert_eq!(text, "Hello, world!");
        assert_eq!(resp.usage.completion_tokens, 3);
    }

    #[test]
    fn test_parse_response_text_delta_multiple() {
        let provider = ChatGptProvider::new();
        // Multiple delta chunks must be concatenated in order.
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Foo\"}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\" \"}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Bar\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"output\":[],",
            "\"usage\":{\"input_tokens\":2,\"output_tokens\":2,\"total_tokens\":4}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.5")
            .expect("parse failed");
        let text = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.as_str(),
            other => panic!("expected Text content, got: {other:?}"),
        };
        assert_eq!(text, "Foo Bar");
    }

    /// #844: verbosity=medium で 1 応答に message アイテムが複数出て
    /// 「本文A → NO_REPLY → 本文A」のロールプレイ軌跡形になる実測ケース。
    /// 無区切り連結（"本文ANO_REPLY本文A"）せず、非センチネルの先頭アイテム "本文A" だけを採用する。
    #[test]
    fn test_parse_response_multi_message_items_skips_no_reply_sentinel() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            // item1: 本文A
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"本文A\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
            "\n",
            // item2: NO_REPLY（センチネル）
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"NO_REPLY\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
            "\n",
            // item3: 本文A（逐語再掲）
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"本文A\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r-844\",\"output\":[],",
            "\"usage\":{\"input_tokens\":5,\"output_tokens\":6,\"total_tokens\":11}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.5")
            .expect("parse failed");
        let text = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.as_str(),
            other => panic!("expected Text content, got: {other:?}"),
        };
        assert_eq!(
            text, "本文A",
            "非センチネルの先頭アイテムだけを採用せず、NO_REPLY を連結して露出させている"
        );
    }

    /// #844: 全アイテムがセンチネルなら NO_REPLY を残す（下流の全文一致で沈黙判定できるように）。
    #[test]
    fn test_parse_response_all_items_no_reply_preserves_sentinel() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"NO_REPLY\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n",
            "\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"NO_REPLY\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r-844b\",\"output\":[],",
            "\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.5")
            .expect("parse failed");
        let text = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.as_str(),
            other => panic!("expected Text content, got: {other:?}"),
        };
        assert_eq!(text.trim(), "NO_REPLY");
    }

    /// #844: センチネルが先頭でも、後続の実体アイテムを採用する（NO_REPLY → 本文B）。
    #[test]
    fn test_parse_response_sentinel_first_picks_later_real_item() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"NO_REPLY\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n",
            "\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\"}}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"本文B\"}\n",
            "\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\"}}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r-844c\",\"output\":[],",
            "\"usage\":{\"input_tokens\":3,\"output_tokens\":3,\"total_tokens\":6}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.5")
            .expect("parse failed");
        let text = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.as_str(),
            other => panic!("expected Text content, got: {other:?}"),
        };
        assert_eq!(text, "本文B");
    }

    #[test]
    fn test_parse_response_empty_no_output() {
        // A response with no delta events and no tool calls → empty content is fine.
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r3\",\"output\":[],",
            "\"usage\":{\"input_tokens\":1,\"output_tokens\":0,\"total_tokens\":1}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.5")
            .expect("parse failed");
        assert_eq!(resp.usage.completion_tokens, 0);
        assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::Stop));
    }

    #[test]
    fn test_parse_response_unicode_delta() {
        // Multibyte characters must be handled correctly.
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"こんにちは\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r4\",\"output\":[],",
            "\"usage\":{\"input_tokens\":1,\"output_tokens\":5,\"total_tokens\":6}}}\n",
            "\n",
        );
        let resp = provider
            .parse_response(sse, "gpt-5.5")
            .expect("parse failed");
        let text = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.as_str(),
            other => panic!("expected Text content, got: {other:?}"),
        };
        assert_eq!(text, "こんにちは");
    }

    /// A system message for the real-API tests. Without it
    /// `build_request_body` emits no `instructions` field and the API
    /// rejects the request with HTTP 400 "Instructions are required".
    fn real_test_system() -> Message {
        Message::system("You are a helpful assistant.")
    }

    #[tokio::test]
    #[ignore]
    async fn test_real_chatgpt_api() {
        // Uses real ~/.codex/auth.json — run with: cargo test -- --ignored
        let provider = ChatGptProvider::new();
        let request = ChatRequest {
            model: TEST_MODEL.to_string(),
            messages: vec![
                real_test_system(),
                Message {
                    role: Role::User,
                    content: Some(MessageContent::Text(
                        "Say exactly: hello from test".to_string(),
                    )),
                    name: None,
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            functions: None,
            function_call: None,
            temperature: None,
            max_tokens: Some(50),
            stop: None,
            stream: Some(false),
            metadata: std::collections::HashMap::new(),
            agent_id: None,
            reasoning_effort: None,
        };
        let response = provider.chat_completion(request).await;
        assert!(response.is_ok(), "API call failed: {:?}", response.err());
        let resp = response.unwrap();
        assert!(!resp.choices.is_empty(), "No choices returned");
        let content = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.clone(),
            _ => panic!("Expected text content"),
        };
        assert!(!content.is_empty(), "Empty response content");
        println!("Response: {}", content);
    }

    /// Build a simple weather function tool used by the real-API tool tests.
    fn weather_tool() -> FunctionDefinition {
        FunctionDefinition {
            name: "get_current_weather".to_string(),
            description: Some("Get the current weather for a given city.".to_string()),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "City name, e.g. Tokyo"}
                },
                "required": ["city"],
                "additionalProperties": false
            }),
        }
    }

    /// Real API: the model can emit a parseable tool call.
    /// Run with: cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn test_real_chatgpt_api_tool_call() {
        let provider = ChatGptProvider::new();
        let request = ChatRequest {
            model: TEST_MODEL.to_string(),
            messages: vec![
                real_test_system(),
                Message::user(
                    "What is the current weather in Tokyo? Call the get_current_weather tool to find out.",
                ),
            ],
            functions: Some(vec![weather_tool()]),
            // Force the call so the test is deterministic.
            function_call: Some(FunctionCallBehavior::Named {
                name: "get_current_weather".to_string(),
            }),
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: Some(false),
            metadata: std::collections::HashMap::new(),
            agent_id: None,
            reasoning_effort: None,
        };
        let response = provider.chat_completion(request).await;
        // On 400 the provider bails with the full HTTP body — surface it here.
        assert!(
            response.is_ok(),
            "tool-call API request failed (inspect for HTTP 400 detail): {:?}",
            response.err()
        );
        let resp = response.unwrap();
        assert!(!resp.choices.is_empty(), "no choices returned");
        assert_eq!(
            resp.choices[0].finish_reason,
            Some(FinishReason::ToolCalls),
            "expected the model to emit a tool call"
        );
        let calls = resp.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("tool_calls must be present");
        assert!(!calls.is_empty(), "tool_calls vec must not be empty");
        let call = &calls[0];
        assert_eq!(
            call.function.name, "get_current_weather",
            "unexpected tool name"
        );
        let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
            .unwrap_or_else(|e| {
                panic!(
                    "tool arguments must be valid JSON ({e}): {}",
                    call.function.arguments
                )
            });
        assert!(
            args.get("city").is_some(),
            "expected a 'city' argument, got: {}",
            call.function.arguments
        );
        println!(
            "Tool call: {} args={}",
            call.function.name, call.function.arguments
        );
    }

    /// Real API: a continuation request after tool execution must NOT 400.
    /// First force a tool call, then send back the tool result as a
    /// function_call_output and assert the model produces a final answer.
    /// Run with: cargo test -- --ignored
    #[tokio::test]
    #[ignore]
    async fn test_real_chatgpt_api_tool_continuation() {
        let provider = ChatGptProvider::new();
        let user = Message::user(
            "What is the current weather in Tokyo? Call the get_current_weather tool.",
        );

        // Phase 1: force the tool call.
        let first = ChatRequest {
            model: TEST_MODEL.to_string(),
            messages: vec![real_test_system(), user.clone()],
            functions: Some(vec![weather_tool()]),
            function_call: Some(FunctionCallBehavior::Named {
                name: "get_current_weather".to_string(),
            }),
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: Some(false),
            metadata: std::collections::HashMap::new(),
            agent_id: None,
            reasoning_effort: None,
        };
        let first_resp = provider
            .chat_completion(first)
            .await
            .unwrap_or_else(|e| panic!("first (tool-call) request failed: {e:?}"));
        let calls = first_resp.choices[0]
            .message
            .tool_calls
            .clone()
            .expect("expected a tool call in the first response");
        assert!(!calls.is_empty(), "tool_calls must not be empty");

        // Phase 2: assistant message carrying the tool calls + tool results.
        let mut assistant = Message::assistant("");
        assistant.tool_calls = Some(calls.clone());

        let mut messages = vec![real_test_system(), user, assistant];
        for c in &calls {
            messages.push(Message::tool(
                c.id.clone(),
                r#"{"temperature_c":22,"condition":"Sunny"}"#,
            ));
        }

        let second = ChatRequest {
            model: TEST_MODEL.to_string(),
            messages,
            functions: Some(vec![weather_tool()]),
            // Let the model produce the final text answer.
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: Some(false),
            metadata: std::collections::HashMap::new(),
            agent_id: None,
            reasoning_effort: None,
        };
        let response = provider.chat_completion(second).await;
        assert!(
            response.is_ok(),
            "continuation request failed — must NOT be HTTP 400 (full error): {:?}",
            response.err()
        );
        let final_resp = response.unwrap();
        let text = final_resp.first_text().unwrap_or("");
        assert!(
            !text.is_empty(),
            "final continuation text must not be empty; finish_reason={:?}",
            final_resp
                .choices
                .first()
                .and_then(|c| c.finish_reason.clone())
        );
        println!("Continuation final text: {}", text);
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
}
