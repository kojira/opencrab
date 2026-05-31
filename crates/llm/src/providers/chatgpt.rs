use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const DEFAULT_MODEL: &str = "gpt-5.5";

/// Expand a leading `~` in a path to the value of the `HOME` environment variable.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/{}", home, rest)
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else {
        path.to_string()
    }
}

#[derive(Debug, Clone)]
pub struct ChatGptProvider {
    client: Client,
    /// Path to auth.json file (default: ~/.codex/auth.json)
    auth_file: String,
    base_url: String,
    default_model: String,
}

impl Default for ChatGptProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatGptProvider {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            client: Client::new(),
            auth_file: format!("{}/.codex/auth.json", home),
            base_url: CHATGPT_BASE_URL.to_string(),
            default_model: DEFAULT_MODEL.to_string(),
        }
    }

    pub fn with_auth_file(mut self, path: impl Into<String>) -> Self {
        let p: String = path.into();
        self.auth_file = expand_tilde(&p);
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    /// Read access_token from auth_file
    fn load_access_token(&self) -> Result<String> {
        let content = std::fs::read_to_string(&self.auth_file)
            .with_context(|| format!("Failed to read auth file: {}", self.auth_file))?;
        let parsed: Value =
            serde_json::from_str(&content).context("Failed to parse auth.json")?;
        let token = parsed["tokens"]["access_token"]
            .as_str()
            .context("tokens.access_token not found in auth.json")?
            .to_string();
        Ok(token)
    }

    fn request_builder(&self, endpoint: &str, token: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, endpoint);
        self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/json")
    }

    /// Build the request body (OpenAI-compatible format)
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
                                ContentPart::ImageUrl { image_url } => serde_json::json!({
                                    "type": "image_url",
                                    "image_url": {"url": image_url.url}
                                }),
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
                        "function": {"name": f.name, "description": f.description, "parameters": f.parameters}
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
                    body["tool_choice"] = serde_json::json!({"type": "function", "function": {"name": name}});
                }
            }
        }
        body
    }

    fn parse_response(&self, body: Value) -> Result<ChatResponse> {
        let id = body["id"].as_str().unwrap_or_default().to_string();
        let model = body["model"].as_str().unwrap_or_default().to_string();
        let created = body["created"].as_i64().unwrap_or(0);
        let usage = if let Some(u) = body.get("usage") {
            Usage {
                prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
                completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
                total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
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
                            .and_then(|tc| serde_json::from_value::<Vec<ToolCall>>(tc.clone()).ok());
                        let finish_reason =
                            c.get("finish_reason").and_then(|fr| match fr.as_str()? {
                                "stop" => Some(FinishReason::Stop),
                                "length" => Some(FinishReason::Length),
                                "function_call" => Some(FinishReason::FunctionCall),
                                "tool_calls" => Some(FinishReason::ToolCalls),
                                "content_filter" => Some(FinishReason::ContentFilter),
                                _ => None,
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
                                cache_control: None,
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
impl LlmProvider for ChatGptProvider {
    fn name(&self) -> &str {
        "chatgpt"
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(vec![
            ModelInfo {
                id: "gpt-5.5".to_string(),
                name: "GPT-5.5".to_string(),
                context_window: 128000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-4o".to_string(),
                name: "GPT-4o".to_string(),
                context_window: 128000,
                supports_function_calling: true,
                supports_vision: true,
            },
            ModelInfo {
                id: "gpt-4.5-preview".to_string(),
                name: "GPT-4.5 Preview".to_string(),
                context_window: 128000,
                supports_function_calling: true,
                supports_vision: true,
            },
        ])
    }

    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        debug!(model = %request.model, "ChatGPT chat completion");
        let token = self.load_access_token()?;
        let body = self.build_request_body(&request);
        let resp = self
            .request_builder("chat/completions", &token)
            .json(&body)
            .send()
            .await
            .context("ChatGPT API request failed")?;
        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .context("failed to parse ChatGPT response")?;
        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            anyhow::bail!("ChatGPT API error ({}): {}", status, error_msg);
        }
        self.parse_response(resp_body)
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "ChatGPT streaming chat completion");
        let token = self.load_access_token()?;
        let mut body = self.build_request_body(&request);
        body["stream"] = serde_json::json!(true);
        let resp = self
            .request_builder("chat/completions", &token)
            .json(&body)
            .send()
            .await
            .context("ChatGPT streaming request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let err_body: Value = resp.json().await.unwrap_or_default();
            let msg = err_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            anyhow::bail!("ChatGPT API error ({}): {}", status, msg);
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
                                                role: delta.get("role").and_then(|r| {
                                                    serde_json::from_value(r.clone()).ok()
                                                }),
                                                content: delta
                                                    .get("content")
                                                    .and_then(|v| v.as_str().map(String::from)),
                                                function_call: delta.get("function_call").and_then(
                                                    |fc| serde_json::from_value(fc.clone()).ok(),
                                                ),
                                                tool_calls: delta.get("tool_calls").and_then(
                                                    |tc| serde_json::from_value(tc.clone()).ok(),
                                                ),
                                            },
                                            finish_reason: c.get("finish_reason").and_then(|fr| {
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
        Ok(self.load_access_token().is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_expand_tilde_basic() {
        let home = std::env::var("HOME").unwrap_or_default();
        assert_eq!(
            expand_tilde("~/.codex/auth.json"),
            format!("{}/.codex/auth.json", home)
        );
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn test_parse_auth_json() {
        let mut file = NamedTempFile::new().expect("failed to create temp file");
        write!(file, r#"{{"tokens":{{"access_token":"test-token-123"}}}}"#)
            .expect("failed to write temp file");
        let path = file.path().to_str().expect("invalid temp path").to_string();
        let provider = ChatGptProvider::new().with_auth_file(path);
        let token = provider.load_access_token();
        assert_eq!(token.unwrap(), "test-token-123");
    }

    #[test]
    fn test_load_access_token_missing_file() {
        let provider = ChatGptProvider::new().with_auth_file("/nonexistent/path/auth.json");
        assert!(provider.load_access_token().is_err());
    }
}
