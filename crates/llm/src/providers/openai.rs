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
        let mut builder = self.client.post(&url)
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
            "messages": self.convert_messages(&request.messages),
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
            // Convert to OpenAI tools format
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
                        "function": { "name": name }
                    });
                }
            }
        }

        body
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<Value> {
        messages.iter().map(|m| self.convert_message(m)).collect()
    }

    fn convert_message(&self, msg: &Message) -> Value {
        let mut obj = serde_json::json!({
            "role": msg.role,
        });

        if let Some(ref content) = msg.content {
            match content {
                MessageContent::Text(text) => {
                    obj["content"] = serde_json::json!(text);
                }
                MessageContent::Image { image_url, .. } => {
                    obj["content"] = serde_json::json!([
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": image_url.url,
                            }
                        }
                    ]);
                }
                MessageContent::Multi(parts) => {
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
                    obj["content"] = serde_json::json!(parts_json);
                }
            }
        }

        if let Some(ref name) = msg.name {
            obj["name"] = serde_json::json!(name);
        }
        if let Some(ref tool_calls) = msg.tool_calls {
            obj["tool_calls"] = serde_json::to_value(tool_calls).unwrap_or_default();
        }
        if let Some(ref tool_call_id) = msg.tool_call_id {
            obj["tool_call_id"] = serde_json::json!(tool_call_id);
        }

        obj
    }

    /// Parse the OpenAI API response into our unified format.
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

                        let content = msg.get("content").and_then(|v| {
                            v.as_str().map(|s| MessageContent::Text(s.to_string()))
                        });

                        let function_call = msg.get("function_call").and_then(|fc| {
                            serde_json::from_value::<FunctionCall>(fc.clone()).ok()
                        });

                        let tool_calls = msg.get("tool_calls").and_then(|tc| {
                            serde_json::from_value::<Vec<ToolCall>>(tc.clone()).ok()
                        });

                        let tool_call_id = msg
                            .get("tool_call_id")
                            .and_then(|v| v.as_str().map(String::from));

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
                                tool_call_id,
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
        let resp_body: Value = resp.json().await.context("failed to parse OpenAI response")?;

        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            anyhow::bail!("OpenAI API error ({}): {}", status, error_msg);
        }

        self.parse_response(resp_body)
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
            anyhow::bail!("OpenAI API error ({}): {}", status, msg);
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
                                                    .and_then(|v| {
                                                        v.as_str().map(String::from)
                                                    }),
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
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await?;
        Ok(resp.status().is_success())
    }
}

