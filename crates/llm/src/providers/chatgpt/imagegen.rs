//! ChatGPT サブスクリプションでの画像生成（D-1076）。
//!
//! Codex CLI の標準画像生成と同じ `POST {base_url}/codex/images/generations` を、
//! チャットと同じ ChatGPT OAuth（Bearer + `chatgpt-account-id`）で叩く。応答は
//! ストリームではない JSON で、`data[0].b64_json` に PNG が入る。
//!
//! router は通さない（retry しない）。1 回の生成が利用枠を消費するので、429 /
//! 4xx / 5xx / 空応答 / タイムアウトはどれもそのまま呼び出し元へ返す。例外は 401 で、
//! チャット経路と同じく 1 回だけトークンをリフレッシュして再送する。

use base64::Engine as _;

use super::*;
use crate::image_generation::{
    GeneratedImage, ImageGenerationError, ImageGenerationRequest, ImageGenerator,
};

/// 画像生成の HTTP 全体タイムアウト（秒）。1 枚 1〜3 分かかるため、チャットの
/// `timeout_secs`（既定 60 秒）とは別のクライアントで持つ。
pub(super) const IMAGEGEN_TIMEOUT_SECS: u64 = 300;
const IMAGEGEN_ENDPOINT: &str = "codex/images/generations";
const IMAGEGEN_MODEL: &str = "gpt-image-2";
const IMAGEGEN_REQUEST_ID_HEADER: &str = "x-codex-imagegen-request-id";

impl ChatGptProvider {
    /// 画像生成の POST を組む。認証ヘッダはチャットの `request_builder` と同じで、
    /// 応答が JSON なので `accept` だけが異なる。
    fn imagegen_request_builder(&self, token: &str, account_id: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, IMAGEGEN_ENDPOINT);
        self.imagegen_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("chatgpt-account-id", account_id)
            .header("originator", "pi")
            .header("accept", "application/json")
            .header("Content-Type", "application/json")
    }

    fn transport_error(&self, e: reqwest::Error) -> ImageGenerationError {
        if e.is_timeout() {
            ImageGenerationError::Timeout(self.imagegen_timeout_secs)
        } else {
            ImageGenerationError::Other(format!("request failed: {e}"))
        }
    }

    /// 画像を 1 枚生成する（`gpt-image-2` / size・quality は auto）。
    pub async fn generate_image(
        &self,
        request: &ImageGenerationRequest,
    ) -> Result<GeneratedImage, ImageGenerationError> {
        let other = |e: anyhow::Error| ImageGenerationError::Other(format!("{e:#}"));
        let body = serde_json::json!({
            "model": IMAGEGEN_MODEL,
            "prompt": request.prompt,
            "background": if request.transparent_background { "transparent" } else { "opaque" },
            "quality": "auto",
            "size": "auto",
        });
        let mut token = self.fresh_access_token().await.map_err(other)?;
        let mut account_id = extract_account_id(&token).map_err(other)?;
        let mut refreshed = false;
        let (status, request_id, text) = loop {
            let resp = self
                .imagegen_request_builder(&token, &account_id)
                .json(&body)
                .send()
                .await
                .map_err(|e| self.transport_error(e))?;
            let status = resp.status();
            let request_id = resp
                .headers()
                .get(IMAGEGEN_REQUEST_ID_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let text = resp.text().await.map_err(|e| self.transport_error(e))?;
            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                tracing::info!("ChatGPT imagegen returned 401; refreshing token and retrying once");
                token = self
                    .refresh_access_token(Some(&token))
                    .await
                    .map_err(other)?;
                account_id = extract_account_id(&token).map_err(other)?;
                continue;
            }
            break (status, request_id, text);
        };
        tracing::info!(
            status = %status,
            body_len = text.len(),
            request_id = request_id.as_deref().unwrap_or(""),
            "ChatGPT imagegen response received"
        );
        if !status.is_success() {
            return Err(classify_error(status, &text));
        }
        parse_success(&text, request_id)
    }
}

/// 非 2xx 応答を分類する。本文は JSON の `error.message` を優先し、無ければ生の本文。
fn classify_error(status: reqwest::StatusCode, text: &str) -> ImageGenerationError {
    let json: Option<Value> = serde_json::from_str(text).ok();
    let error = json.as_ref().map(|j| j.get("error").unwrap_or(j));
    let message = error
        .and_then(|e| e.get("message").or_else(|| e.get("detail")))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| text.trim().chars().take(500).collect());
    let kind = error
        .and_then(|e| e.get("type").or_else(|| e.get("code")))
        .and_then(Value::as_str)
        .unwrap_or("");
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || kind == "usage_limit_reached" {
        let resets_at = error
            .and_then(|e| e.get("resets_at"))
            .or_else(|| json.as_ref().and_then(|j| j.get("resets_at")))
            .and_then(Value::as_i64);
        return ImageGenerationError::UsageLimit { message, resets_at };
    }
    if status.is_client_error() {
        ImageGenerationError::Rejected {
            status: status.as_u16(),
            message,
        }
    } else {
        ImageGenerationError::Server {
            status: status.as_u16(),
            message,
        }
    }
}

fn parse_success(
    text: &str,
    request_id: Option<String>,
) -> Result<GeneratedImage, ImageGenerationError> {
    let json: Value = serde_json::from_str(text)
        .map_err(|e| ImageGenerationError::Other(format!("invalid JSON response: {e}")))?;
    let first = json["data"]
        .as_array()
        .and_then(|d| d.first())
        .ok_or(ImageGenerationError::EmptyData)?;
    let b64 = first["b64_json"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(ImageGenerationError::EmptyData)?;
    let png_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| ImageGenerationError::Other(format!("invalid b64_json: {e}")))?;
    if png_bytes.is_empty() {
        return Err(ImageGenerationError::EmptyData);
    }
    Ok(GeneratedImage {
        png_bytes,
        revised_prompt: first["revised_prompt"].as_str().map(str::to_string),
        usage: json.get("usage").filter(|u| !u.is_null()).cloned(),
        request_id,
    })
}

#[async_trait]
impl ImageGenerator for ChatGptProvider {
    async fn generate_image(
        &self,
        request: &ImageGenerationRequest,
    ) -> Result<GeneratedImage, ImageGenerationError> {
        ChatGptProvider::generate_image(self, request).await
    }
}
