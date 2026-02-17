use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const OLLAMA_DEFAULT_URL: &str = "http://localhost:11434";

/// Ollama local LLM provider.
#[derive(Debug, Clone)]
pub struct OllamaProvider {
    client: Client,
    base_url: String,
}

impl OllamaProvider {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            base_url: OLLAMA_DEFAULT_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Convert unified messages to Ollama chat API format.
    /// Ollama's /api/chat endpoint accepts OpenAI-compatible message format.
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

                // Include images for vision models
                if let Some(MessageContent::Image { image_url, .. }) = &msg.content {
                    m["images"] = serde_json::json!([image_url.url]);
                }

                m
            })
            .collect();

        let mut body = serde_json::json!({
            "model": request.model,
            "messages": messages,
            "stream": stream,
        });

        // Options
        let mut options = serde_json::json!({});
        if let Some(temp) = request.temperature {
            options["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = request.max_tokens {
            options["num_predict"] = serde_json::json!(max);
        }
        if let Some(ref stop) = request.stop {
            options["stop"] = serde_json::json!(stop);
        }
        if options.as_object().map_or(false, |o| !o.is_empty()) {
            body["options"] = options;
        }

        // Tools (Ollama supports OpenAI-compatible tool format)
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

        body
    }

    /// Parse Ollama chat response into unified format.
    fn parse_response(&self, body: Value) -> Result<ChatResponse> {
        let model = body["model"].as_str().unwrap_or_default().to_string();

        let msg = &body["message"];
        let role = match msg["role"].as_str().unwrap_or("assistant") {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => Role::Assistant,
        };

        let content = msg
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| MessageContent::Text(s.to_string()));

        let tool_calls = msg.get("tool_calls").and_then(|tc| {
            serde_json::from_value::<Vec<ToolCall>>(tc.clone()).ok()
        });

        let finish_reason = if body["done"].as_bool().unwrap_or(false) {
            Some(FinishReason::Stop)
        } else {
            None
        };

        // Ollama returns token counts in eval_count / prompt_eval_count
        let prompt_tokens = body["prompt_eval_count"].as_u64().unwrap_or(0) as u32;
        let completion_tokens = body["eval_count"].as_u64().unwrap_or(0) as u32;

        Ok(ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role,
                    content,
                    name: None,
                    function_call: None,
                    tool_calls,
                    tool_call_id: None,
                },
                finish_reason,
            }],
            usage: Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            },
            created: chrono::Utc::now().timestamp(),
        })
    }
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to list Ollama models")?;

        let body: Value = resp.json().await.context("failed to parse Ollama model list")?;
        let models = body["models"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let name = m["name"].as_str()?.to_string();
                        Some(ModelInfo {
                            id: name.clone(),
                            name,
                            context_window: 4096, // Default; varies per model
                            supports_function_calling: false,
                            supports_vision: false,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }

    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        debug!(model = %request.model, "Ollama chat completion");

        let url = format!("{}/api/chat", self.base_url);
        let body = self.build_request_body(&request, false);

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Ollama API request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama API error ({}): {}", status, err_text);
        }

        let resp_body: Value = resp.json().await.context("failed to parse Ollama response")?;
        self.parse_response(resp_body)
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "Ollama streaming chat completion");

        let url = format!("{}/api/chat", self.base_url);
        let body = self.build_request_body(&request, true);

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Ollama streaming request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Ollama API error ({}): {}", status, err_text);
        }

        let model = request.model.clone();
        let stream = resp.bytes_stream().map(move |chunk| {
            let chunk = chunk.context("stream chunk error")?;
            let text = String::from_utf8_lossy(&chunk);
            let model = model.clone();

            // Ollama streams newline-delimited JSON objects
            let mut content_text = String::new();
            let mut done = false;

            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(parsed) = serde_json::from_str::<Value>(line) {
                    if let Some(c) = parsed["message"]["content"].as_str() {
                        content_text.push_str(c);
                    }
                    if parsed["done"].as_bool().unwrap_or(false) {
                        done = true;
                    }
                }
            }

            Ok(ChatStreamDelta {
                id: uuid::Uuid::new_v4().to_string(),
                model,
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
                    finish_reason: if done {
                        Some(FinishReason::Stop)
                    } else {
                        None
                    },
                }],
            })
        });

        Ok(Box::pin(stream))
    }

    fn supports_function_calling(&self) -> bool {
        // Some Ollama models support tools, but not all
        false
    }

    fn supports_vision(&self) -> bool {
        // Some Ollama models (llava, etc.) support vision
        false
    }

    async fn health_check(&self) -> Result<bool> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self.client.get(&url).send().await?;
        Ok(resp.status().is_success())
    }
}
