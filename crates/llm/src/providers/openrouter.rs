use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const OPENROUTER_API_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter provider.
///
/// OpenRouter provides a unified API compatible with OpenAI's format,
/// but requires additional headers for attribution (HTTP-Referer, X-Title).
#[derive(Debug, Clone)]
pub struct OpenRouterProvider {
    client: Client,
    api_key: String,
    base_url: String,
    /// HTTP-Referer header for OpenRouter attribution.
    referer: Option<String>,
    /// X-Title header for OpenRouter attribution.
    title: Option<String>,
}

impl OpenRouterProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: OPENROUTER_API_URL.to_string(),
            referer: None,
            title: None,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Set the HTTP-Referer header for OpenRouter.
    pub fn with_referer(mut self, referer: impl Into<String>) -> Self {
        self.referer = Some(referer.into());
        self
    }

    /// Set the X-Title header for OpenRouter.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    fn request_builder(&self, endpoint: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, endpoint);
        let mut builder = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");

        if let Some(ref referer) = self.referer {
            builder = builder.header("HTTP-Referer", referer);
        }
        if let Some(ref title) = self.title {
            builder = builder.header("X-Title", title);
        }

        builder
    }

    /// Build the request body (OpenAI-compatible format).
    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let messages = super::openai_compat::messages_to_json(&request.messages);

        let mut body = serde_json::json!({
            "model": request.model,
            "messages": messages,
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        if let Some(ref stop) = request.stop {
            body["stop"] = serde_json::json!(stop);
        }
        if let Some(ref functions) = request.functions {
            let tools: Vec<Value> = functions
                .iter()
                .map(|f| {
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
                        "function": {"name": name}
                    });
                }
            }
        }

        body
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    fn name(&self) -> &str {
        "openrouter"
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/models", self.base_url);
        let mut builder = self.client.get(&url);

        if let Some(ref referer) = self.referer {
            builder = builder.header("HTTP-Referer", referer);
        }
        if let Some(ref title) = self.title {
            builder = builder.header("X-Title", title);
        }

        let resp = builder
            .send()
            .await
            .context("failed to list OpenRouter models")?;

        let body: Value = resp
            .json()
            .await
            .context("failed to parse OpenRouter model list")?;

        let models = body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m["id"].as_str()?.to_string();
                        let name = m["name"].as_str().unwrap_or(&id).to_string();
                        let ctx = m["context_length"].as_u64().unwrap_or(4096) as u32;
                        Some(ModelInfo {
                            id,
                            name,
                            context_window: ctx,
                            supports_function_calling: true,
                            supports_vision: false,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }

    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        debug!(model = %request.model, "OpenRouter chat completion");

        let body = self.build_request_body(&request);
        let resp = self
            .request_builder("chat/completions")
            .json(&body)
            .send()
            .await
            .context("OpenRouter API request failed")?;

        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .context("failed to parse OpenRouter response")?;

        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(crate::error::api_error("OpenRouter", status, error_msg));
        }

        Ok(super::openai_compat::parse_chat_response(&resp_body, ""))
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "OpenRouter streaming chat completion");

        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::json!(true);

        let resp = self
            .request_builder("chat/completions")
            .json(&body)
            .send()
            .await
            .context("OpenRouter streaming request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body: Value = resp.json().await.unwrap_or_default();
            let msg = err_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(crate::error::api_error("OpenRouter", status, msg));
        }

        // チャンク境界を跨いでバッファし、完全な行ごとに1イベントとして処理する。
        // OpenRouter は keep-alive コメント（`: OPENROUTER PROCESSING`）を単独チャンクで
        // 送ってくるが、それらはデータ行でないためスキップし、エラーにしない。
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
        let resp = self.client.get(&url).send().await?;
        Ok(resp.status().is_success())
    }
}
