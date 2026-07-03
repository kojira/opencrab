use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const LLAMACPP_DEFAULT_URL: &str = "http://localhost:8080";

/// llama.cpp server provider (llama-server / llama-cpp-python).
#[derive(Debug, Clone)]
pub struct LlamaCppProvider {
    name: String,
    client: Client,
    base_url: String,
}

impl LlamaCppProvider {
    pub fn new() -> Self {
        Self {
            name: "llamacpp".to_string(),
            client: Client::new(),
            base_url: LLAMACPP_DEFAULT_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// llama.cpp server exposes an OpenAI-compatible /v1/chat/completions endpoint.
    fn build_request_body(&self, request: &ChatRequest, stream: bool) -> Value {
        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|msg| {
                let role = match msg.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                };

                let content = msg.text_content().unwrap_or("").to_string();

                let mut m = serde_json::json!({
                    "role": role,
                    "content": content,
                });

                if let Some(ref name) = msg.name {
                    m["name"] = serde_json::json!(name);
                }
                if let Some(ref tool_call_id) = msg.tool_call_id {
                    m["tool_call_id"] = serde_json::json!(tool_call_id);
                }

                m
            })
            .collect();

        let mut body = serde_json::json!({
            "messages": messages,
            "stream": stream,
        });

        // llama.cpp may or may not use the model field, but include it for compatibility
        if !request.model.is_empty() {
            body["model"] = serde_json::json!(request.model);
        }

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max);
        }
        if let Some(ref stop) = request.stop {
            body["stop"] = serde_json::json!(stop);
        }

        body
    }

}

impl Default for LlamaCppProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmProvider for LlamaCppProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        // llama.cpp typically serves a single model.
        // Try the /v1/models endpoint if available.
        let url = format!("{}/v1/models", self.base_url);
        let resp = self.client.get(&url).send().await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let body: Value = r.json().await.unwrap_or_default();
                let models = body["data"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| {
                                let id = m["id"].as_str()?.to_string();
                                Some(ModelInfo {
                                    name: id.clone(),
                                    id,
                                    context_window: 4096,
                                    supports_function_calling: false,
                                    supports_vision: false,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(models)
            }
            _ => {
                // Fallback: return a placeholder
                Ok(vec![ModelInfo {
                    id: "local".to_string(),
                    name: "Local llama.cpp model".to_string(),
                    context_window: 4096,
                    supports_function_calling: false,
                    supports_vision: false,
                }])
            }
        }
    }

    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        debug!(model = %request.model, "llama.cpp chat completion");

        let url = format!("{}/v1/chat/completions", self.base_url);
        let body = self.build_request_body(&request, false);

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("llama.cpp API request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(crate::error::api_error("llama.cpp", status, err_text));
        }

        let resp_body: Value = resp
            .json()
            .await
            .context("failed to parse llama.cpp response")?;
        Ok(super::openai_compat::parse_chat_response(&resp_body))
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "llama.cpp streaming chat completion");

        let url = format!("{}/v1/chat/completions", self.base_url);
        let body = self.build_request_body(&request, true);

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("llama.cpp streaming request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_text = resp.text().await.unwrap_or_default();
            return Err(crate::error::api_error("llama.cpp", status, err_text));
        }

        // チャンク境界を跨いでバッファし、完全な行ごとに1イベントとして処理する。
        // `data:` 行の delta 抽出は openai_compat に一本化（[DONE]/コメント行はスキップ）。
        let stream = crate::providers::sse::line_stream(resp.bytes_stream()).filter_map(
            move |line_res| {
                let out = match line_res {
                    Err(e) => Some(Err(e)),
                    Ok(line) => super::openai_compat::delta_from_sse_line(&line).map(Ok),
                };
                futures::future::ready(out)
            },
        );

        Ok(Box::pin(stream))
    }

    fn supports_function_calling(&self) -> bool {
        false
    }

    fn supports_vision(&self) -> bool {
        false
    }

    async fn health_check(&self) -> Result<bool> {
        let url = format!("{}/health", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(_) => {
                // Try alternative endpoint
                let url = format!("{}/v1/models", self.base_url);
                let resp = self.client.get(&url).send().await?;
                Ok(resp.status().is_success())
            }
        }
    }
}
