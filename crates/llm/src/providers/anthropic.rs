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
                    // 複数の system メッセージは改行で連結する（上書きすると
                    // 先行する system 指示が失われる）。
                    if let Some(text) = msg.text_content() {
                        system_prompt = Some(match system_prompt.take() {
                            Some(existing) if !existing.is_empty() => {
                                format!("{existing}\n\n{text}")
                            }
                            _ => text.to_string(),
                        });
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
                    let has_tool_calls = msg.tool_calls.as_ref().map_or(false, |tc| !tc.is_empty());
                    if has_tool_calls {
                        // Build content array with text parts + tool_use blocks
                        let mut content_blocks: Vec<Value> = Vec::new();
                        // Add text content if present
                        if let Some(text) = msg.text_content() {
                            if !text.is_empty() {
                                content_blocks.push(serde_json::json!({
                                    "type": "text",
                                    "text": text,
                                }));
                            }
                        }
                        // Add tool_use blocks
                        for tc in msg.tool_calls.as_ref().unwrap() {
                            let input: Value = serde_json::from_str(&tc.function.arguments)
                                .unwrap_or(serde_json::json!({}));
                            content_blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.function.name,
                                "input": input,
                            }));
                        }
                        messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": content_blocks,
                        }));
                    } else {
                        let content = self.convert_content_to_anthropic(msg);
                        messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": content,
                        }));
                    }
                }
                Role::Tool => {
                    // Anthropic uses tool_result blocks inside a user message.
                    // Multiple consecutive tool results will be merged in post-processing.
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

        // Post-process: merge consecutive user messages that contain only tool_result blocks
        let mut merged: Vec<Value> = Vec::new();
        for msg in messages {
            let is_tool_result_user = msg["role"] == "user"
                && msg["content"].as_array().map_or(false, |arr| {
                    !arr.is_empty() && arr.iter().all(|b| b["type"] == "tool_result")
                });
            if is_tool_result_user {
                // Check if the previous message is also a tool_result user message
                let should_merge = merged.last().map_or(false, |prev: &Value| {
                    prev["role"] == "user"
                        && prev["content"].as_array().map_or(false, |arr| {
                            !arr.is_empty() && arr.iter().all(|b| b["type"] == "tool_result")
                        })
                });
                if should_merge {
                    // Append tool_result blocks to previous message
                    let prev = merged.last_mut().unwrap();
                    let new_blocks = msg["content"].as_array().unwrap().clone();
                    let prev_content = prev["content"].as_array_mut().unwrap();
                    prev_content.extend(new_blocks);
                } else {
                    merged.push(msg);
                }
            } else {
                merged.push(msg);
            }
        }
        let messages = merged;

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
                    let mut tool = serde_json::json!({
                        "name": f.name,
                        "description": f.description,
                        "input_schema": f.parameters,
                    });
                    // Anthropic のプロンプトキャッシュ: ツール定義に cache_control を出力する。
                    if let Some(ref cc) = f.cache_control {
                        tool["cache_control"] = cc.clone();
                    }
                    tool
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
                    + u["output_tokens"].as_u64().unwrap_or(0))
                    as u32,
                cache_read_input_tokens: u["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32,
                cache_creation_input_tokens: u["cache_creation_input_tokens"].as_u64().unwrap_or(0)
                    as u32,
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
            cache_control: None,
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
        let resp_body: Value = resp
            .json()
            .await
            .context("failed to parse Anthropic response")?;

        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(crate::error::api_error("Anthropic", status, error_msg));
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
            return Err(crate::error::api_error("Anthropic", status, msg));
        }

        let model = request.model.clone();
        // チャンク境界を跨いでバッファし、SSEイベントを行単位で処理する。
        // content_block_delta ごとに1デルタを emit する（チャンク内での結合はしない）。
        let stream = crate::providers::sse::line_stream(resp.bytes_stream()).filter_map(
            move |line_res| {
                let model = model.clone();
                let out = match line_res {
                    Err(e) => Some(Err(e)),
                    Ok(line) => {
                        let line = line.trim();
                        if let Some(data) = line.strip_prefix("data:") {
                            let data = data.trim();
                            match serde_json::from_str::<Value>(data) {
                                Ok(parsed) => match parsed["type"].as_str() {
                                    Some("message_start") => {
                                        let id = parsed["message"]["id"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .to_string();
                                        Some(Ok(ChatStreamDelta {
                                            id,
                                            model,
                                            choices: vec![StreamChoice {
                                                index: 0,
                                                delta: DeltaMessage {
                                                    role: None,
                                                    content: None,
                                                    function_call: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: None,
                                            }],
                                        }))
                                    }
                                    Some("content_block_delta") => parsed["delta"]["text"]
                                        .as_str()
                                        .map(|text| {
                                            Ok(ChatStreamDelta {
                                                id: String::new(),
                                                model,
                                                choices: vec![StreamChoice {
                                                    index: 0,
                                                    delta: DeltaMessage {
                                                        role: None,
                                                        content: Some(text.to_string()),
                                                        function_call: None,
                                                        tool_calls: None,
                                                    },
                                                    finish_reason: None,
                                                }],
                                            })
                                        }),
                                    _ => None,
                                },
                                Err(_) => None,
                            }
                        } else {
                            None
                        }
                    }
                };
                futures::future::ready(out)
            },
        );

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

        let resp = self.request_builder("messages").json(&body).send().await?;

        // 200 or 401 both mean the endpoint is reachable
        Ok(resp.status().is_success() || resp.status().as_u16() == 401)
    }
}
