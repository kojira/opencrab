use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Anthropic Claude API provider.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: ANTHROPIC_API_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn request_builder(&self, endpoint: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, endpoint);
        self.client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("Content-Type", "application/json")
    }

    /// Convert unified messages to Anthropic Messages API format.
    /// Anthropic separates the system message from the messages array.
    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let mut system_prompt: Option<String> = None;
        let mut messages: Vec<Value> = Vec::new();

        for msg in &request.messages {
            match msg.role {
                Role::System => {
                    if let Some(text) = msg.text_content() {
                        system_prompt = Some(text.to_string());
                    }
                }
                Role::User => {
                    let content = self.convert_content_to_anthropic(msg);
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": content,
                    }));
                }
                Role::Assistant => {
                    let content = self.convert_content_to_anthropic(msg);
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": content,
                    }));
                }
                Role::Tool => {
                    // Anthropic uses tool_result blocks
                    let tool_call_id = msg.tool_call_id.as_deref().unwrap_or("");
                    let text = msg.text_content().unwrap_or("");
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool_call_id,
                            "content": text,
                        }],
                    }));
                }
            }
        }

        let max_tokens = request.max_tokens.unwrap_or(4096);

        let mut body = serde_json::json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": max_tokens,
        });

        if let Some(system) = system_prompt {
            body["system"] = serde_json::json!(system);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(ref stop) = request.stop {
            body["stop_sequences"] = serde_json::json!(stop);
        }
        if let Some(ref functions) = request.functions {
            let tools: Vec<Value> = functions
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "name": f.name,
                        "description": f.description,
                        "input_schema": f.parameters,
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(tools);
        }

        body
    }

    fn convert_content_to_anthropic(&self, msg: &Message) -> Value {
        match &msg.content {
            Some(MessageContent::Text(text)) => serde_json::json!(text),
            Some(MessageContent::Image { image_url, .. }) => {
                // Anthropic uses base64 image format or URL-based source
                serde_json::json!([{
                    "type": "image",
                    "source": {
                        "type": "url",
                        "url": image_url.url,
                    }
                }])
            }
            Some(MessageContent::Multi(parts)) => {
                let blocks: Vec<Value> = parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => {
                            serde_json::json!({"type": "text", "text": text})
                        }
                        ContentPart::ImageUrl { image_url } => {
                            serde_json::json!({
                                "type": "image",
                                "source": {
                                    "type": "url",
                                    "url": image_url.url,
                                }
                            })
                        }
                    })
                    .collect();
                serde_json::json!(blocks)
            }
            None => serde_json::json!(""),
        }
    }

    /// Parse Anthropic Messages API response into unified format.
    fn parse_response(&self, body: Value) -> Result<ChatResponse> {
        let id = body["id"].as_str().unwrap_or_default().to_string();
        let model = body["model"].as_str().unwrap_or_default().to_string();

        let usage = if let Some(u) = body.get("usage") {
            Usage {
                prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0) as u32,
                completion_tokens: u["output_tokens"].as_u64().unwrap_or(0) as u32,
                total_tokens: (u["input_tokens"].as_u64().unwrap_or(0)
                    + u["output_tokens"].as_u64().unwrap_or(0)) as u32,
            }
        } else {
            Usage::default()
        };

        // Build message from content blocks
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        if let Some(content_arr) = body["content"].as_array() {
            for block in content_arr {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = block["text"].as_str() {
                            text_parts.push(text.to_string());
                        }
                    }
                    Some("tool_use") => {
                        let tc_id = block["id"].as_str().unwrap_or_default().to_string();
                        let name = block["name"].as_str().unwrap_or_default().to_string();
                        let arguments = block["input"].to_string();
                        tool_calls.push(ToolCall {
                            id: tc_id,
                            call_type: "function".to_string(),
                            function: FunctionCall { name, arguments },
                        });
                    }
                    _ => {}
                }
            }
        }

        let content = if text_parts.is_empty() {
            None
        } else {
            Some(MessageContent::Text(text_parts.join("")))
        };

        let finish_reason = match body["stop_reason"].as_str() {
            Some("end_turn") | Some("stop_sequence") => Some(FinishReason::Stop),
            Some("max_tokens") => Some(FinishReason::Length),
            Some("tool_use") => Some(FinishReason::ToolCalls),
            _ => None,
        };

        let message = Message {
            role: Role::Assistant,
            content,
            name: None,
            function_call: None,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            tool_call_id: None,
        };

        Ok(ChatResponse {
            id,
            model,
            choices: vec![Choice {
                index: 0,
                message,
                finish_reason,
            }],
            usage,
            created: chrono::Utc::now().timestamp(),
        })
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        // Anthropic does not have a model listing endpoint; return known models.
        Ok(vec![
            ModelInfo {
                id: "claude-sonnet-4-20250514".to_string(),
                name: "Claude Sonnet 4".to_string(),
                context_window: 200_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "claude-3-5-sonnet-20241022".to_string(),
                name: "Claude 3.5 Sonnet".to_string(),
                context_window: 200_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "claude-3-opus-20240229".to_string(),
                name: "Claude 3 Opus".to_string(),
                context_window: 200_000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "claude-3-haiku-20240307".to_string(),
                name: "Claude 3 Haiku".to_string(),
                context_window: 200_000,
                supports_function_calling: true,
                supports_vision: true,
            },
        ])
    }

    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        debug!(model = %request.model, "Anthropic chat completion");

        let body = self.build_request_body(&request);
        let resp = self
            .request_builder("messages")
            .json(&body)
            .send()
            .await
            .context("Anthropic API request failed")?;

        let status = resp.status();
        let resp_body: Value = resp.json().await.context("failed to parse Anthropic response")?;

        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            anyhow::bail!("Anthropic API error ({}): {}", status, error_msg);
        }

        self.parse_response(resp_body)
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "Anthropic streaming chat completion");

        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::json!(true);

        let resp = self
            .request_builder("messages")
            .json(&body)
            .send()
            .await
            .context("Anthropic streaming request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body: Value = resp.json().await.unwrap_or_default();
            let msg = err_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            anyhow::bail!("Anthropic API error ({}): {}", status, msg);
        }

        let model = request.model.clone();
        let stream = resp.bytes_stream().map(move |chunk| {
            let chunk = chunk.context("stream chunk error")?;
            let text = String::from_utf8_lossy(&chunk);
            let model = model.clone();

            let mut content_text = String::new();
            let mut msg_id = String::new();

            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(parsed) = serde_json::from_str::<Value>(data) {
                        match parsed["type"].as_str() {
                            Some("message_start") => {
                                if let Some(id) = parsed["message"]["id"].as_str() {
                                    msg_id = id.to_string();
                                }
                            }
                            Some("content_block_delta") => {
                                if let Some(text) = parsed["delta"]["text"].as_str() {
                                    content_text.push_str(text);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            Ok(ChatStreamDelta {
                id: msg_id,
                model: model.clone(),
                choices: vec![StreamChoice {
                    index: 0,
                    delta: DeltaMessage {
                        role: None,
                        content: if content_text.is_empty() {
                            None
                        } else {
                            Some(content_text)
                        },
                        function_call: None,
                        tool_calls: None,
                    },
                    finish_reason: None,
                }],
            })
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
        // Anthropic does not have a dedicated health endpoint.
        // Send a minimal request to verify connectivity.
        let body = serde_json::json!({
            "model": "claude-3-haiku-20240307",
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 1,
        });

        let resp = self
            .request_builder("messages")
            .json(&body)
            .send()
            .await?;

        // 200 or 401 both mean the endpoint is reachable
        Ok(resp.status().is_success() || resp.status().as_u16() == 401)
    }
}
