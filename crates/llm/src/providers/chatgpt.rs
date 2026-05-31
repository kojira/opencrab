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

/// Decode a base64url string (no padding required) into bytes.
fn base64url_decode(input: &str) -> anyhow::Result<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &c in input.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = val(c).ok_or_else(|| anyhow::anyhow!("invalid base64url character"))? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

/// Extract the `chatgpt_account_id` from a JWT access token's claims.
fn extract_account_id(token: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        anyhow::bail!("invalid JWT: expected at least 2 dot-separated parts");
    }
    let payload_bytes =
        base64url_decode(parts[1]).context("failed to base64url-decode JWT payload")?;
    let payload: serde_json::Value =
        serde_json::from_slice(&payload_bytes).context("failed to parse JWT payload JSON")?;
    let account_id = payload["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .context("chatgpt_account_id not found in JWT claims")?
        .to_string();
    Ok(account_id)
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

    fn request_builder(
        &self,
        endpoint: &str,
        token: &str,
        account_id: &str,
    ) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base_url, endpoint);
        self.client
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("chatgpt-account-id", account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "pi")
            .header("accept", "text/event-stream")
            .header("Content-Type", "application/json")
    }

    /// Convert a message's content into the Responses API content value.
    fn message_content_value(content: &Option<MessageContent>) -> Option<Value> {
        match content {
            Some(MessageContent::Text(text)) => Some(serde_json::json!(text)),
            Some(MessageContent::Image { image_url, .. }) => Some(serde_json::json!([{
                "type": "image_url",
                "image_url": {"url": image_url.url}
            }])),
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
                Some(serde_json::json!(parts_json))
            }
            None => None,
        }
    }

    /// Build the request body in the Responses API format.
    fn build_request_body(&self, request: &ChatRequest, stream: bool) -> Value {
        let mut system_prompts: Vec<String> = Vec::new();
        let mut input: Vec<Value> = Vec::new();

        for msg in &request.messages {
            if msg.role == Role::System {
                if let Some(MessageContent::Text(text)) = &msg.content {
                    system_prompts.push(text.clone());
                } else if let Some(content) = Self::message_content_value(&msg.content) {
                    if let Some(s) = content.as_str() {
                        system_prompts.push(s.to_string());
                    }
                }
                continue;
            }

            let role = match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let mut m = serde_json::json!({"role": role});
            if let Some(content) = Self::message_content_value(&msg.content) {
                m["content"] = content;
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
            input.push(m);
        }

        let mut body = serde_json::json!({
            "model": request.model,
            "store": false,
            "stream": stream,
            "input": input,
            "text": {"verbosity": "medium"},
            "include": ["reasoning.encrypted_content"],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });

        if !system_prompts.is_empty() {
            body["instructions"] = serde_json::json!(system_prompts.join("\n\n"));
        }

        if let Some(ref functions) = request.functions {
            let tools: Vec<Value> = functions
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "type": "function",
                        "name": f.name,
                        "description": f.description,
                        "parameters": f.parameters,
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
                    body["tool_choice"] =
                        serde_json::json!({"type": "function", "name": name});
                }
            }
        }

        body
    }

    /// Parse a fully-collected SSE response body into a `ChatResponse`.
    fn parse_response(&self, sse_text: &str, model: &str) -> Result<ChatResponse> {
        let mut content = String::new();
        let mut id = String::new();
        let mut usage = Usage::default();

        for line in sse_text.lines() {
            let line = line.trim();
            let data = match line.strip_prefix("data:") {
                Some(d) => d.trim(),
                None => continue,
            };
            if data == "[DONE]" {
                continue;
            }
            let parsed: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match parsed["type"].as_str().unwrap_or("") {
                "response.output_text.delta" => {
                    if let Some(delta) = parsed["delta"].as_str() {
                        content.push_str(delta);
                    }
                }
                "response.completed" | "response.done" => {
                    if let Some(rid) = parsed["response"]["id"].as_str() {
                        id = rid.to_string();
                    }
                    let u = &parsed["response"]["usage"];
                    usage = Usage {
                        prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0) as u32,
                        completion_tokens: u["output_tokens"].as_u64().unwrap_or(0) as u32,
                        total_tokens: u["total_tokens"].as_u64().unwrap_or(0) as u32,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                    };
                }
                "error" => {
                    let msg = parsed["message"]
                        .as_str()
                        .or_else(|| parsed["error"]["message"].as_str())
                        .unwrap_or("unknown error");
                    anyhow::bail!("ChatGPT API error: {}", msg);
                }
                _ => {}
            }
        }

        Ok(ChatResponse {
            id,
            model: model.to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant(content),
                finish_reason: Some(FinishReason::Stop),
            }],
            usage,
            created: 0,
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
        let account_id = extract_account_id(&token)?;
        let body = self.build_request_body(&request, true);
        let resp = self
            .request_builder("codex/responses", &token, &account_id)
            .json(&body)
            .send()
            .await
            .context("ChatGPT API request failed")?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .context("failed to read ChatGPT response body")?;
        if !status.is_success() {
            anyhow::bail!("ChatGPT API error ({}): {}", status, text);
        }
        self.parse_response(&text, &request.model)
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "ChatGPT streaming chat completion");
        let token = self.load_access_token()?;
        let account_id = extract_account_id(&token)?;
        let body = self.build_request_body(&request, true);
        let resp = self
            .request_builder("codex/responses", &token, &account_id)
            .json(&body)
            .send()
            .await
            .context("ChatGPT streaming request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("ChatGPT API error ({}): {}", status, text);
        }
        let request_model = request.model.clone();
        let stream = resp.bytes_stream().map(move |chunk| {
            let chunk = chunk.context("stream chunk error")?;
            let text = String::from_utf8_lossy(&chunk);
            let mut last_delta: Option<ChatStreamDelta> = None;
            for line in text.lines() {
                let line = line.trim();
                let data = match line.strip_prefix("data:") {
                    Some(d) => d.trim(),
                    None => continue,
                };
                if data == "[DONE]" {
                    continue;
                }
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                match parsed["type"].as_str().unwrap_or("") {
                    "response.output_text.delta" => {
                        let delta_text =
                            parsed["delta"].as_str().unwrap_or_default().to_string();
                        last_delta = Some(ChatStreamDelta {
                            id: String::new(),
                            model: request_model.clone(),
                            choices: vec![StreamChoice {
                                index: 0,
                                delta: DeltaMessage {
                                    role: None,
                                    content: Some(delta_text),
                                    function_call: None,
                                    tool_calls: None,
                                },
                                finish_reason: None,
                            }],
                        });
                    }
                    "response.completed" | "response.done" => {
                        last_delta = Some(ChatStreamDelta {
                            id: String::new(),
                            model: request_model.clone(),
                            choices: vec![StreamChoice {
                                index: 0,
                                delta: DeltaMessage {
                                    role: None,
                                    content: Some(String::new()),
                                    function_call: None,
                                    tool_calls: None,
                                },
                                finish_reason: Some(FinishReason::Stop),
                            }],
                        });
                    }
                    _ => {}
                }
            }
            Ok(last_delta.unwrap_or(ChatStreamDelta {
                id: String::new(),
                model: request_model.clone(),
                choices: vec![],
            }))
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

    fn b64url_encode(data: &[u8]) -> String {
        const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHA[((n >> 18) & 63) as usize] as char);
            out.push(ALPHA[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHA[((n >> 6) & 63) as usize] as char);
            }
            if chunk.len() > 2 {
                out.push(ALPHA[(n & 63) as usize] as char);
            }
        }
        out
    }

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

    #[test]
    fn test_base64url_decode_roundtrip() {
        let samples: &[&[u8]] = &[b"", b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"];
        for s in samples {
            let encoded = b64url_encode(s);
            assert_eq!(base64url_decode(&encoded).unwrap(), s.to_vec());
        }
    }

    #[test]
    fn test_extract_account_id() {
        let payload = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-xyz-123" }
        });
        let payload_b64 = b64url_encode(payload.to_string().as_bytes());
        let token = format!("header.{}.signature", payload_b64);
        assert_eq!(extract_account_id(&token).unwrap(), "acct-xyz-123");
    }

    #[test]
    fn test_extract_account_id_invalid() {
        assert!(extract_account_id("notajwt").is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn test_real_chatgpt_api() {
        // Uses real ~/.codex/auth.json — run with: cargo test -- --ignored
        let provider = ChatGptProvider::new();
        let request = ChatRequest {
            model: "gpt-4o".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: Some(MessageContent::Text("Say exactly: hello from test".to_string())),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
                cache_control: None,
            }],
            functions: None,
            function_call: None,
            temperature: None,
            max_tokens: Some(50),
            stop: None,
            stream: Some(false),
            metadata: std::collections::HashMap::new(),
            agent_id: None,
        };
        let response = provider.chat_completion(request).await;
        assert!(response.is_ok(), "API call failed: {:?}", response.err());
        let resp = response.unwrap();
        assert!(!resp.choices.is_empty(), "No choices returned");
        let content = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.clone(),
            _ => panic!("Expected text content"),
        };
        assert!(!content.is_empty(), "Empty response content");
        println!("Response: {}", content);
    }
}
