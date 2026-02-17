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

                let mut m = serde_json::json!({"role": role});

                match &msg.content {
                    Some(MessageContent::Text(text)) => {
                        m["content"] = serde_json::json!(text);
                    }
                    Some(MessageContent::Image { image_url, .. }) => {
                        m["content"] = serde_json::json!([{
                            "type": "image_url",
                            "image_url": {"url": image_url.url}
                        }]);
                    }
                    Some(MessageContent::Multi(parts)) => {
                        let parts_json: Vec<Value> = parts
                            .iter()
                            .map(|p| match p {
                                ContentPart::Text { text } => {
                                    serde_json::json!({"type": "text", "text": text})
                                }
                                ContentPart::ImageUrl { image_url } => {
                                    serde_json::json!({
                                        "type": "image_url",
                                        "image_url": {"url": image_url.url}
                                    })
                                }
                            })
                            .collect();
                        m["content"] = serde_json::json!(parts_json);
                    }
                    None => {}
                }

                if let Some(ref name) = msg.name {
                    m["name"] = serde_json::json!(name);
                }
                if let Some(ref tool_calls) = msg.tool_calls {
                    m["tool_calls"] = serde_json::to_value(tool_calls).unwrap_or_default();
                }
                if let Some(ref tool_call_id) = msg.tool_call_id {
                    m["tool_call_id"] = serde_json::json!(tool_call_id);
                }

                m
            })
            .collect();

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

    /// Parse response (OpenAI-compatible format).
    fn parse_response(&self, body: Value) -> Result<ChatResponse> {
        let id = body["id"].as_str().unwrap_or_default().to_string();
        let model = body["model"].as_str().unwrap_or_default().to_string();
        let created = body["created"].as_i64().unwrap_or(0);

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
                            .map(|s| MessageContent::Text(s.to_string()));

                        let function_call = msg
                            .get("function_call")
                            .and_then(|fc| serde_json::from_value::<FunctionCall>(fc.clone()).ok());

                        let tool_calls = msg
                            .get("tool_calls")
                            .and_then(|tc| {
                                serde_json::from_value::<Vec<ToolCall>>(tc.clone()).ok()
                            });

                        let finish_reason = c.get("finish_reason").and_then(|fr| {
                            match fr.as_str()? {
                                "stop" => Some(FinishReason::Stop),
                                "length" => Some(FinishReason::Length),
                                "function_call" => Some(FinishReason::FunctionCall),
                                "tool_calls" => Some(FinishReason::ToolCalls),
                                "content_filter" => Some(FinishReason::ContentFilter),
                                _ => None,
                            }
                        });

                        Choice {
                            index: c["index"].as_u64().unwrap_or(0) as u32,
                            message: Message {
                                role,
                                content,
                                name: None,
                                function_call,
                                tool_calls,
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
            anyhow::bail!("OpenRouter API error ({}): {}", status, error_msg);
        }

        self.parse_response(resp_body)
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
            anyhow::bail!("OpenRouter API error ({}): {}", status, msg);
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
                        let model = parsed["model"].as_str().unwrap_or_default().to_string();
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
                                                function_call: delta
                                                    .get("function_call")
                                                    .and_then(|fc| {
                                                        serde_json::from_value(fc.clone()).ok()
                                                    }),
                                                tool_calls: delta
                                                    .get("tool_calls")
                                                    .and_then(|tc| {
                                                        serde_json::from_value(tc.clone()).ok()
                                                    }),
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
