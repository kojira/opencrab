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
    client: Client,
    base_url: String,
}

impl LlamaCppProvider {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            base_url: LLAMACPP_DEFAULT_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
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

    /// Parse OpenAI-compatible response from llama.cpp server.
    fn parse_response(&self, body: Value) -> Result<ChatResponse> {
        let id = body["id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let model = body["model"]
            .as_str()
            .unwrap_or("local")
            .to_string();
        let created = body["created"].as_i64().unwrap_or_else(|| chrono::Utc::now().timestamp());

        let usage = if let Some(u) = body.get("usage") {
            Usage {
                prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
                total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
            }
        } else {
            Usage::default()
        };

        let choices = body["choices"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|c| {
                        let msg = &c["message"];
                        let role = match msg["role"].as_str().unwrap_or("assistant") {
                            "system" => Role::System,
                            "user" => Role::User,
                            "assistant" => Role::Assistant,
                            "tool" => Role::Tool,
                            _ => Role::Assistant,
                        };

                        let content = msg
                            .get("content")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| MessageContent::Text(s.to_string()));

                        let finish_reason = c.get("finish_reason").and_then(|fr| {
                            match fr.as_str()? {
                                "stop" => Some(FinishReason::Stop),
                                "length" => Some(FinishReason::Length),
                                _ => None,
                            }
                        });

                        Choice {
                            index: c["index"].as_u64().unwrap_or(0) as u32,
                            message: Message {
                                role,
                                content,
                                name: None,
                                function_call: None,
                                tool_calls: None,
                                tool_call_id: None,
                            },
                            finish_reason,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(ChatResponse {
            id,
            model,
            choices,
            usage,
            created,
        })
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
        "llamacpp"
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
            anyhow::bail!("llama.cpp API error ({}): {}", status, err_text);
        }

        let resp_body: Value = resp
            .json()
            .await
            .context("failed to parse llama.cpp response")?;
        self.parse_response(resp_body)
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
            anyhow::bail!("llama.cpp API error ({}): {}", status, err_text);
        }

        let stream = resp.bytes_stream().map(move |chunk| {
            let chunk = chunk.context("stream chunk error")?;
            let text = String::from_utf8_lossy(&chunk);

            let mut last_delta = None;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line == "data: [DONE]" {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(parsed) = serde_json::from_str::<Value>(data) {
                        let id = parsed["id"].as_str().unwrap_or_default().to_string();
                        let model = parsed["model"]
                            .as_str()
                            .unwrap_or("local")
                            .to_string();

                        let choices = parsed["choices"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .map(|c| {
                                        let delta = &c["delta"];
                                        StreamChoice {
                                            index: c["index"].as_u64().unwrap_or(0) as u32,
                                            delta: DeltaMessage {
                                                role: delta
                                                    .get("role")
                                                    .and_then(|r| {
                                                        serde_json::from_value(r.clone()).ok()
                                                    }),
                                                content: delta
                                                    .get("content")
                                                    .and_then(|v| v.as_str().map(String::from)),
                                                function_call: None,
                                                tool_calls: None,
                                            },
                                            finish_reason: c
                                                .get("finish_reason")
                                                .and_then(|fr| {
                                                    serde_json::from_value(fr.clone()).ok()
                                                }),
                                        }
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();

                        last_delta = Some(ChatStreamDelta { id, model, choices });
                    }
                }
            }

            last_delta.ok_or_else(|| anyhow::anyhow!("no parseable SSE data in chunk"))
        });

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
