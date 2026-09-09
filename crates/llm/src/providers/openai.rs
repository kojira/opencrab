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

/// チャット補完リクエスト全体の上限時間（秒）。超過で reqwest が timeout エラーを
/// 返し、ターンが fail loud に落ちる（#667）。
///
/// なぜ「総時間」で「アイドル間隔」でないか: 本番経路は非ストリーミング単発 POST で、
/// hermit-shell は上流 Anthropic を内部でストリーミングしつつ **finalMessage まで集約して
/// 1 回で返す**（生成中は opencrab→hermit 間にバイトが流れない）。よってチャンク間の
/// アイドル timeout は正当な無音生成を誤殺してしまうため、この経路で効くのは総時間 timeout
/// だけになる。
///
/// なぜ 600 秒か: 出力上限（hermit:claude-opus-5 で 128K）まで許すが、実測の生成速度は
/// 79 tok/s（#676: completion 4096 tok = 51.9 秒）で、実運用の大きな報告でも数分で完了する。
/// 10 分を超える単発生成は極めて稀（数万トークン超の一括応答）で、上流の無応答（週次上限到達の
/// 429 沈黙 sleep・プロセスクラッシュ後のソケット沈黙）と区別して確実に有限で切るための値。
/// タイムアウトは 4xx でないため router の既存リトライ対象に含まれる。
const CHAT_TIMEOUT_SECS: u64 = 600;
/// 接続確立の timeout（秒）。生成の長さとは無関係。接続すら張れない状態を即検知する。
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// チャット補完用の HTTP クライアントを組む（総時間 timeout 付き・#667）。
fn build_client(timeout_secs: u64) -> Client {
    Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .build()
        // 起動時 1 回の構築。ここで失敗した client へ無言退化すると timeout 無しに戻り
        // 本 PR の主旨（無応答を有限で切る）を裏切るため fail loud に落とす（#667）。
        .expect("failed to build HTTP client with timeout")
}

/// GPT-5 系 / o シリーズ（推論モデル）を chat/completions で呼ぶときの制約を
/// 判定する。これらのモデルは:
/// - `temperature` は既定値 (1) のみ受け付け、他の値は 400 を返す
/// - `max_tokens` は不可で `max_completion_tokens` を使う（推論トークンも消費）
/// - `reasoning_effort` を受け付ける
///
/// `*-chat*` 変種（例: gpt-5-chat-latest）は非推論で従来どおり temperature/
/// max_tokens を受け付けるため除外する。
fn is_reasoning_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.contains("chat") {
        return false;
    }
    m.starts_with("gpt-5") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
}

/// 非既定 `temperature` を載せると 400 になるモデルか。
///
/// GPT-5 / o シリーズは既存の推論判定。Claude は公式 cutoff（Opus 4.7 以降、
/// および Claude 5 系。Sonnet 5 は QC 実測で temperature deprecated）。
/// 世代で判定し、個別モデル ID の列挙はしない。`is_reasoning_model` には畳まない
/// （Claude は `max_tokens` / `reasoning_effort` の扱いが違う）。
fn omits_temperature(model: &str) -> bool {
    is_reasoning_model(model) || claude_rejects_temperature(model)
}

fn claude_rejects_temperature(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    let name = m.rsplit(['/', ':']).next().unwrap_or(m.as_str());
    if name.contains("mythos-preview") {
        return true;
    }
    match claude_generation(name) {
        Some((major, minor)) => major >= 5 || (major == 4 && minor >= 7),
        None => false,
    }
}

/// Claude の世代 `(major, minor)`。読めなければ `None`（温度は従来どおり送る）。
///
/// - `claude-sonnet-5` / `claude-opus-5` → (5, 0)
/// - `claude-opus-4-7` / `claude-sonnet-4-6` → (4, 7) / (4, 6)
/// - `claude-3-5-sonnet-20241022` → (3, 5)
fn claude_generation(model: &str) -> Option<(u32, u32)> {
    let rest = model.strip_prefix("claude-")?;
    let parts: Vec<&str> = rest.split('-').collect();
    let first = parts.first()?;
    if let Ok(major) = first.parse::<u32>() {
        let minor = parts
            .get(1)
            .and_then(|p| parse_claude_version_part(p))
            .unwrap_or(0);
        return Some((major, minor));
    }
    let nums: Vec<u32> = parts
        .iter()
        .skip(1)
        .filter_map(|p| parse_claude_version_part(p))
        .collect();
    let major = *nums.first()?;
    let minor = nums.get(1).copied().unwrap_or(0);
    Some((major, minor))
}

fn parse_claude_version_part(part: &str) -> Option<u32> {
    let n = part.parse::<u32>().ok()?;
    // 日付サフィックス（20250929）は世代ではない
    if n >= 20_000_000 {
        None
    } else {
        Some(n)
    }
}

/// OpenAI API provider.
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    base_url: String,
    org_id: Option<String>,
    /// GPT-5 系 / o シリーズに付与する reasoning_effort（"minimal"|"low"|"medium"
    /// |"high"）。空/未設定なら送らない（サーバ既定 = medium）。
    reasoning_effort: Option<String>,
    /// テレメトリ用の表示名。ルーティングキーは router 登録時に別途決まるため、
    /// これは接続先を人間に見せるためのラベルにすぎない（既定は形式名 "openai"）。
    name: String,
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: build_client(CHAT_TIMEOUT_SECS),
            api_key: api_key.into(),
            base_url: "https://api.openai.com/v1".to_string(),
            org_id: None,
            reasoning_effort: None,
            name: "openai".to_string(),
        }
    }

    /// 表示名を上書きする（同じ形式の接続先を別名で登録するとき）。
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_org_id(mut self, org_id: impl Into<String>) -> Self {
        self.org_id = Some(org_id.into());
        self
    }

    /// GPT-5 系 / o シリーズに付与する reasoning_effort を設定する。
    /// 空文字は「未設定」として扱う。
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        let s = effort.into();
        self.reasoning_effort = if s.is_empty() { None } else { Some(s) };
        self
    }

    /// Build the request with auth headers.
    fn request_builder(&self, endpoint: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, endpoint);
        let mut builder = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");

        if let Some(ref org) = self.org_id {
            builder = builder.header("OpenAI-Organization", org);
        }

        builder
    }

    /// Build the JSON body for a chat completion request.
    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let mut body = serde_json::json!({
            "model": request.model,
            "messages": super::openai_compat::messages_to_json(&request.messages),
        });

        let reasoning = is_reasoning_model(&request.model);

        if let Some(temp) = request.temperature {
            // エンジンは 0.7/0.0 を常に載せる。非対応モデルへ送ると 400 になるので
            // ここで落とす（再送フォールバックではない。wire を先に正しくする）。
            if !omits_temperature(&request.model) {
                body["temperature"] = serde_json::json!(temp);
            }
        }
        if let Some(max) = request.max_tokens {
            // 推論モデルは max_tokens 不可 → max_completion_tokens を使う。
            // 注: 内部推論トークンもこの予算を消費するため、小さすぎると出力が
            // 途中で切れうる（呼び出し側の予算設定の問題で、リクエストは成功する）。
            if reasoning {
                body["max_completion_tokens"] = serde_json::json!(max);
            } else {
                body["max_tokens"] = serde_json::json!(max);
            }
        }
        if reasoning {
            // per-request（エージェント個別）を優先し、無ければ構築時の既定。
            if let Some(effort) = request
                .reasoning_effort
                .as_deref()
                .or(self.reasoning_effort.as_deref())
            {
                body["reasoning_effort"] = serde_json::json!(effort);
            }
        }
        if let Some(ref stop) = request.stop {
            body["stop"] = serde_json::json!(stop);
        }
        if let Some(stream) = request.stream {
            body["stream"] = serde_json::json!(stream);
        }
        if let Some(ref functions) = request.functions {
            let tools: Vec<Value> = functions
                .iter()
                .map(|f| {
                    // cache_control は Anthropic 固有のフィールドで、OpenAI は未知の
                    // パラメータとして 400 で拒否するため、ここでは出力しない。
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": f.name,
                            "description": f.description,
                            "parameters": f.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(tools);
        }
        if let Some(ref fc) = request.function_call {
            match fc {
                FunctionCallBehavior::Mode(mode) => {
                    body["tool_choice"] = serde_json::json!(mode);
                }
                FunctionCallBehavior::Named { name } => {
                    body["tool_choice"] = serde_json::json!({
                        "type": "function",
                        "function": { "name": name }
                    });
                }
            }
        }

        body
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/models", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .context("failed to list OpenAI models")?;

        let body: Value = resp.json().await.context("failed to parse model list")?;
        let models = body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m["id"].as_str()?.to_string();
                        Some(ModelInfo {
                            name: id.clone(),
                            id,
                            context_window: 128_000,
                            supports_function_calling: true,
                            supports_vision: true,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }

    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        debug!(model = %request.model, "OpenAI chat completion");

        let body = self.build_request_body(&request);
        let resp = self
            .request_builder("chat/completions")
            .json(&body)
            .send()
            .await
            .context("OpenAI API request failed")?;

        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .context("failed to parse OpenAI response")?;

        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(crate::error::api_error("OpenAI", status, error_msg));
        }

        Ok(super::openai_compat::parse_chat_response(&resp_body, ""))
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "OpenAI streaming chat completion");

        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::json!(true);

        let resp = self
            .request_builder("chat/completions")
            .json(&body)
            .send()
            .await
            .context("OpenAI streaming request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body: Value = resp.json().await.unwrap_or_default();
            let msg = err_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(crate::error::api_error("OpenAI", status, msg));
        }

        // チャンク境界を跨いでバッファし、完全な行ごとに1イベントとして処理する。
        // `data:` 行の delta 抽出は openai_compat に一本化（[DONE]/コメント行はスキップ）。
        let stream =
            crate::providers::sse::line_stream(resp.bytes_stream()).filter_map(move |line_res| {
                let out = match line_res {
                    Err(e) => Some(Err(e)),
                    Ok(line) => super::openai_compat::delta_from_sse_line(&line).map(Ok),
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
        let url = format!("{}/models", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await?;
        Ok(resp.status().is_success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_reasoning_model() {
        assert!(is_reasoning_model("gpt-5.6"));
        assert!(is_reasoning_model("gpt-5.6-sol"));
        assert!(is_reasoning_model("gpt-5.6-terra"));
        assert!(is_reasoning_model("gpt-5.5"));
        assert!(is_reasoning_model("o1"));
        assert!(is_reasoning_model("o3-mini"));
        // chat 変種と従来モデルは非推論扱い
        assert!(!is_reasoning_model("gpt-5-chat-latest"));
        assert!(!is_reasoning_model("gpt-4o"));
        assert!(!is_reasoning_model("gpt-4o-mini"));
    }

    #[test]
    fn test_gpt5_body_omits_temperature_uses_max_completion_tokens() {
        let p = OpenAiProvider::new("k").with_reasoning_effort("high");
        let req = ChatRequest::new("gpt-5.6", vec![Message::user("hi")])
            .with_temperature(0.7)
            .with_max_tokens(4096);
        let body = p.build_request_body(&req);
        // GPT-5 系: temperature は送らない、max は max_completion_tokens に
        assert!(
            body.get("temperature").is_none(),
            "temperature must be omitted for gpt-5"
        );
        assert!(
            body.get("max_tokens").is_none(),
            "max_tokens must not be sent for gpt-5"
        );
        assert_eq!(body["max_completion_tokens"], 4096);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn test_per_request_reasoning_effort_overrides_provider_default() {
        let p = OpenAiProvider::new("k").with_reasoning_effort("low");
        // request 側（エージェント個別）が provider 既定より優先される
        let mut req = ChatRequest::new("gpt-5.6", vec![Message::user("hi")]);
        req.reasoning_effort = Some("high".to_string());
        let body = p.build_request_body(&req);
        assert_eq!(body["reasoning_effort"], "high");

        // request 側が無ければ provider 既定
        let req2 = ChatRequest::new("gpt-5.6", vec![Message::user("hi")]);
        let body2 = p.build_request_body(&req2);
        assert_eq!(body2["reasoning_effort"], "low");
    }

    #[test]
    fn test_gpt4o_body_keeps_temperature_and_max_tokens() {
        let p = OpenAiProvider::new("k").with_reasoning_effort("high");
        let req = ChatRequest::new("gpt-4o", vec![Message::user("hi")])
            .with_temperature(0.7)
            .with_max_tokens(4096);
        let body = p.build_request_body(&req);
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["max_tokens"], 4096);
        // 非推論モデルには reasoning_effort を付けない
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn test_claude_generation_cutoff() {
        assert_eq!(claude_generation("claude-sonnet-5"), Some((5, 0)));
        assert_eq!(claude_generation("claude-opus-5"), Some((5, 0)));
        assert_eq!(claude_generation("claude-opus-4-7"), Some((4, 7)));
        assert_eq!(claude_generation("claude-sonnet-4-6"), Some((4, 6)));
        assert_eq!(
            claude_generation("claude-3-5-sonnet-20241022"),
            Some((3, 5))
        );
        assert_eq!(
            claude_generation("claude-sonnet-4-5-20250929"),
            Some((4, 5))
        );
        assert!(claude_generation("gpt-4o").is_none());
    }

    #[test]
    fn test_claude_sonnet_5_body_omits_temperature_keeps_max_tokens() {
        let p = OpenAiProvider::new("k").with_reasoning_effort("high");
        let req = ChatRequest::new("claude-sonnet-5", vec![Message::user("hi")])
            .with_temperature(0.7)
            .with_max_tokens(4096);
        let body = p.build_request_body(&req);
        assert!(
            body.get("temperature").is_none(),
            "temperature must be omitted for claude-sonnet-5"
        );
        // Claude は推論モデルではない。max_tokens のまま、reasoning_effort は付けない。
        assert_eq!(body["max_tokens"], 4096);
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn test_claude_opus_5_body_omits_temperature() {
        let p = OpenAiProvider::new("k");
        let req = ChatRequest::new("claude-opus-5", vec![Message::user("hi")])
            .with_temperature(0.7)
            .with_max_tokens(128000);
        let body = p.build_request_body(&req);
        assert!(body.get("temperature").is_none());
        assert_eq!(body["max_tokens"], 128000);
    }

    #[test]
    fn test_legacy_claude_body_keeps_temperature() {
        let p = OpenAiProvider::new("k");
        for model in [
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-3-haiku-20240307",
        ] {
            let req = ChatRequest::new(model, vec![Message::user("hi")])
                .with_temperature(0.7)
                .with_max_tokens(1024);
            let body = p.build_request_body(&req);
            assert_eq!(
                body["temperature"], 0.7,
                "{model} still accepts temperature"
            );
            assert_eq!(body["max_tokens"], 1024);
        }
    }

    /// リクエストを受けてから `delay` 待って 200 を返すモック（timeout 検証用）。
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

    /// #667: 総時間 timeout が実際に client へ効いていることを確認する。無応答の上流を
    /// 有限で切る（fail loud）ための肝なので、定数の保持ではなく client の挙動で見る。
    #[tokio::test]
    async fn test_chat_timeout_is_applied_to_the_http_client() {
        let url = spawn_slow_mock(Duration::from_millis(1500)).await;

        // 短い timeout の client は読み切る前に timeout する。
        let short = build_client(1);
        let err = short.get(&url).send().await.unwrap_err();
        assert!(err.is_timeout(), "1 秒なら timeout するはず: {err}");

        // 十分長い timeout の client は読み切れる。
        let long = build_client(10);
        let resp = long.get(&url).send().await.expect("10 秒なら読み切れる");
        assert!(resp.status().is_success());
    }
}
