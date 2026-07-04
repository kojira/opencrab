use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

/// OpenAI API provider.
#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    base_url: String,
    org_id: Option<String>,
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: "https://api.openai.com/v1".to_string(),
            org_id: None,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_org_id(mut self, org_id: impl Into<String>) -> Self {
        self.org_id = Some(org_id.into());
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

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
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
        "openai"
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
