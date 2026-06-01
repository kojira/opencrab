use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;
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
    reasoning_effort: Option<String>,
    include_encrypted_content: bool,
    max_output_tokens: Option<u32>,
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
            client: Client::builder()
                .timeout(Duration::from_secs(60))
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            auth_file: format!("{}/.codex/auth.json", home),
            base_url: CHATGPT_BASE_URL.to_string(),
            default_model: DEFAULT_MODEL.to_string(),
            reasoning_effort: Some("low".to_string()),
            include_encrypted_content: false,
            max_output_tokens: Some(8192),
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

    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        let s: String = effort.into();
        if s.is_empty() {
            self.reasoning_effort = None;
        } else {
            // Higher reasoning effort produces more reasoning tokens, so size the
            // output budget accordingly. An explicit `with_max_output_tokens` call
            // (e.g. from config) applied afterwards still overrides this.
            self.max_output_tokens = Some(match s.as_str() {
                "high" => 32000,
                _ => 25000, // "low" / "medium" / anything else
            });
            self.reasoning_effort = Some(s);
        }
        self
    }

    pub fn with_include_encrypted_content(mut self, v: bool) -> Self {
        self.include_encrypted_content = v;
        self
    }

    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = Some(tokens);
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
        tracing::warn!("chatgpt build_request_body: messages count={}, system_prompts will be extracted", request.messages.len());
        let mut input: Vec<Value> = Vec::new();

        tracing::debug!(message_count = request.messages.len(), "build_request_body: received messages");
        for msg in &request.messages {
            tracing::debug!(role = ?msg.role, "build_request_body: message role");
        }

        for msg in &request.messages {
            if msg.role == Role::System {
                tracing::debug!(
                    role = "system",
                    content_is_some = msg.content.is_some(),
                    "build_request_body: processing system message"
                );
                if let Some(MessageContent::Text(text)) = &msg.content {
                    tracing::debug!(text_len = text.len(), "build_request_body: system message is Text, adding to system_prompts");
                    system_prompts.push(text.clone());
                } else if let Some(content) = Self::message_content_value(&msg.content) {
                    if let Some(s) = content.as_str() {
                        tracing::debug!(str_len = s.len(), "build_request_body: system message content converted to str via message_content_value");
                        system_prompts.push(s.to_string());
                    } else {
                        tracing::warn!(
                            content_type = ?&msg.content,
                            "build_request_body: system message content is not a string after message_content_value conversion, SKIPPING"
                        );
                    }
                } else {
                    tracing::warn!(
                        content_is_none = msg.content.is_none(),
                        "build_request_body: system message content is None or could not be converted, SKIPPING"
                    );
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
            "tool_choice": "auto",
            "parallel_tool_calls": true,
        });

        if let Some(value) = &self.reasoning_effort {
            body["reasoning"] = serde_json::json!({"effort": value});
        }

        // NOTE: max_output_tokens is NOT supported by the chatgpt Responses API
        // (returns 400 "Unsupported parameter: max_output_tokens"). Omit from body.
        // The field is kept internally for potential future use.

        if self.include_encrypted_content {
            body["include"] = serde_json::json!(["reasoning.encrypted_content"]);
        }

        tracing::warn!("chatgpt build_request_body: system_prompts count={}", system_prompts.len());
        if system_prompts.is_empty() {
            tracing::warn!(
                total_messages = request.messages.len(),
                "build_request_body: system_prompts is EMPTY! instructions field will NOT be set -> API will return 400 Bad Request"
            );
        }

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

        debug!(
            model = %request.model,
            stream = stream,
            input_count = input.len(),
            system_prompt_count = system_prompts.len(),
            has_tools = request.functions.is_some(),
            body = %body,
            "chatgpt build_request_body"
        );

        body
    }

    /// Build a `ToolCall` from a Responses API `function_call` output item.
    ///
    /// The item looks like:
    /// `{"type":"function_call","id":"fc_..","call_id":"call_..","name":"..","arguments":"{..}"}`.
    /// Returns `None` if the item is not a function call or is missing a name.
    fn tool_call_from_item(item: &Value) -> Option<ToolCall> {
        if item["type"].as_str() != Some("function_call") {
            return None;
        }
        let name = item["name"].as_str()?.to_string();
        // Prefer `call_id` (referenced by subsequent tool-result messages),
        // falling back to the item `id`.
        let id = item["call_id"]
            .as_str()
            .or_else(|| item["id"].as_str())
            .unwrap_or_default()
            .to_string();
        let arguments = item["arguments"].as_str().unwrap_or("").to_string();
        Some(ToolCall {
            id,
            call_type: "function".to_string(),
            function: FunctionCall { name, arguments },
        })
    }

    /// Render tool calls into the `<function_calls>` XML block that opencrab's
    /// engine parses via `parse_xml_tool_calls`. Each tool call becomes an
    /// `<invoke name="...">` element whose JSON-object arguments are expanded
    /// into per-parameter tags (matching the codex provider's encoding).
    fn tool_calls_to_xml(tool_calls: &[ToolCall]) -> String {
        let mut out = String::from("<function_calls>\n");
        for call in tool_calls {
            out.push_str(&format!("<invoke name=\"{}\">\n", call.function.name));
            match serde_json::from_str::<Value>(&call.function.arguments) {
                Ok(Value::Object(map)) => {
                    for (key, value) in map {
                        let rendered = match value {
                            Value::String(s) => s,
                            other => other.to_string(),
                        };
                        out.push_str(&format!("<{key}>{rendered}</{key}>\n"));
                    }
                }
                // Fall back to emitting the raw arguments if they are not an object.
                _ => {
                    out.push_str(&format!(
                        "<arguments>{}</arguments>\n",
                        call.function.arguments
                    ));
                }
            }
            out.push_str("</invoke>\n");
        }
        out.push_str("</function_calls>");
        out
    }

    /// Parse a fully-collected SSE response body into a `ChatResponse`.
    fn parse_response(&self, sse_text: &str, model: &str) -> Result<ChatResponse> {
        let mut content = String::new();
        let mut id = String::new();
        let mut usage = Usage::default();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        // [parse_response DEBUG] temporary diagnostic logging (remove later)
        {
            let chars: Vec<char> = sse_text.chars().collect();
            let head: String = chars.iter().take(1000).collect();
            let tail: String = if chars.len() > 500 {
                chars[chars.len() - 500..].iter().collect()
            } else {
                chars.iter().collect()
            };
            tracing::warn!("[parse_response DEBUG] sse_text head(1000)=\n{}", head);
            tracing::warn!("[parse_response DEBUG] sse_text tail(500)=\n{}", tail);
        }
        tracing::warn!(
            "[parse_response DEBUG] sse_text total lines = {}",
            sse_text.lines().count()
        );
        let mut dbg_data_line_count: usize = 0;
        let mut dbg_json_ok_count: usize = 0;
        let mut dbg_delta_event_count: usize = 0;
        let mut current_event = String::new();

        for line in sse_text.lines() {
            let line = line.trim();
            if let Some(ev) = line.strip_prefix("event:") {
                current_event = ev.trim().to_string();
                continue;
            }
            let data = match line.strip_prefix("data:") {
                Some(d) => d.trim(),
                None => continue,
            };
            dbg_data_line_count += 1;
            if data == "[DONE]" {
                current_event.clear();
                continue;
            }
            let parsed: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => {
                    current_event.clear();
                    continue;
                }
            };
            dbg_json_ok_count += 1;
            // Effective event type: prefer parsed["type"], fall back to current_event.
            let effective_event = parsed["type"].as_str().unwrap_or(&current_event);
            match effective_event {
                "response.output_text.delta" => {
                    if let Some(delta) = parsed["delta"].as_str() {
                        dbg_delta_event_count += 1;
                        let dbg_delta_head: String = delta.chars().take(50).collect();
                        tracing::warn!(
                            "[parse_response DEBUG] delta event #{} first50={:?}",
                            dbg_delta_event_count, dbg_delta_head
                        );
                        content.push_str(delta);
                    }
                }
                "response.output_item.done" => {
                    // Incremental completion of a single output item; capture any
                    // function call here in case the final output array is absent.
                    if let Some(tc) = Self::tool_call_from_item(&parsed["item"]) {
                        if !tool_calls.iter().any(|existing| existing.id == tc.id) {
                            tool_calls.push(tc);
                        }
                    }
                }
                "response.completed" | "response.done" => {
                    if let Some(rid) = parsed["response"]["id"].as_str() {
                        id = rid.to_string();
                    }
                    // The final output array carries the complete tool calls.
                    if let Some(output) = parsed["response"]["output"].as_array() {
                        for item in output {
                            if let Some(tc) = Self::tool_call_from_item(item) {
                                if !tool_calls.iter().any(|existing| existing.id == tc.id) {
                                    tool_calls.push(tc);
                                }
                            }
                        }
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
            current_event.clear();
        }

        tracing::warn!(
            "[parse_response DEBUG] data: prefixed lines = {}",
            dbg_data_line_count
        );
        tracing::warn!(
            "[parse_response DEBUG] serde_json::from_str OK count = {}",
            dbg_json_ok_count
        );
        tracing::warn!(
            "[parse_response DEBUG] output_text.delta event count = {}",
            dbg_delta_event_count
        );
        tracing::warn!(
            "[parse_response DEBUG] final content length: bytes={} chars={}",
            content.len(),
            content.chars().count()
        );

        let finish_reason = if tool_calls.is_empty() {
            FinishReason::Stop
        } else {
            FinishReason::ToolCalls
        };

        // When tool calls are present, expose them in the content as a
        // `<function_calls>` XML block so the engine's `parse_xml_tool_calls`
        // can recover them, while also keeping the structured `tool_calls`.
        let message = if tool_calls.is_empty() {
            Message::assistant(content)
        } else {
            let xml = Self::tool_calls_to_xml(&tool_calls);
            let combined = if content.trim().is_empty() {
                xml
            } else {
                format!("{content}\n{xml}")
            };
            let mut m = Message::assistant(combined);
            m.tool_calls = Some(tool_calls);
            m
        };

        Ok(ChatResponse {
            id,
            model: model.to_string(),
            choices: vec![Choice {
                index: 0,
                message,
                finish_reason: Some(finish_reason),
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
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        tracing::warn!(
            model = %request.model,
            has_instructions = body.get("instructions").is_some(),
            instructions_len = body["instructions"].as_str().map(|s| s.len()).unwrap_or(0),
            input_count = body["input"].as_array().map(|a| a.len()).unwrap_or(0),
            has_reasoning = body.get("reasoning").is_some(),
            reasoning_effort = body["reasoning"]["effort"].as_str().unwrap_or("none"),
            body_len = body_str.len(),
            "ChatGPT chat_completion: sending request"
        );
        let max_retries = 3u32;
        let mut last_error = String::new();

        for attempt in 0..max_retries {
            if attempt > 0 {
                let backoff_secs = 1u64 << (attempt - 1); // 1, 2, 4
                tracing::warn!(
                    attempt,
                    backoff_secs,
                    last_error = %last_error,
                    "ChatGPT chat_completion: retrying after error"
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            }

            let resp_result = self
                .request_builder("codex/responses", &token, &account_id)
                .json(&body)
                .send()
                .await;

            let resp = match resp_result {
                Ok(r) => r,
                Err(e) => {
                    last_error = format!("request failed: {e}");
                    tracing::warn!(attempt, error = %e, "ChatGPT chat_completion: network error");
                    continue;
                }
            };

            let status = resp.status();
            let text = match resp.text().await {
                Ok(t) => t,
                Err(e) => {
                    last_error = format!("failed to read body: {e}");
                    tracing::warn!(attempt, error = %e, "ChatGPT chat_completion: body read error");
                    continue;
                }
            };

            tracing::warn!(status = %status, body_len = text.len(), "ChatGPT chat_completion response received");

            if !status.is_success() {
                last_error = format!("HTTP {}: (body_len={})", status, text.len());
                tracing::warn!(status = %status, body = %text, "ChatGPT chat_completion error response");
                // Don't retry on 4xx client errors (except 429)
                if status.as_u16() >= 400 && status.as_u16() < 500 && status.as_u16() != 429 {
                    anyhow::bail!("ChatGPT API error ({}): {}", status, text);
                }
                continue;
            }

            tracing::warn!(body_len = text.len(), "ChatGPT chat_completion parsing response");
            let result = self.parse_response(&text, &request.model);
            tracing::warn!(success = result.is_ok(), "ChatGPT chat_completion parse result");
            return result;
        }

        anyhow::bail!("ChatGPT API request failed after {} attempts: {}", max_retries, last_error)
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "ChatGPT streaming chat completion");
        let token = self.load_access_token()?;
        let account_id = extract_account_id(&token)?;
        let body = self.build_request_body(&request, true);
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        tracing::warn!(
            model = %request.model,
            has_instructions = body.get("instructions").is_some(),
            instructions_len = body["instructions"].as_str().map(|s| s.len()).unwrap_or(0),
            input_count = body["input"].as_array().map(|a| a.len()).unwrap_or(0),
            has_reasoning = body.get("reasoning").is_some(),
            reasoning_effort = body["reasoning"]["effort"].as_str().unwrap_or("none"),
            body_len = body_str.len(),
            "ChatGPT chat_completion_stream: sending request"
        );
        let max_retries = 3u32;
        let mut resp = None;
        let mut last_error = String::new();

        for attempt in 0..max_retries {
            if attempt > 0 {
                let backoff_secs = 1u64 << (attempt - 1);
                tracing::warn!(
                    attempt,
                    backoff_secs,
                    last_error = %last_error,
                    "ChatGPT chat_completion_stream: retrying after error"
                );
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            }

            let resp_result = self
                .request_builder("codex/responses", &token, &account_id)
                .json(&body)
                .send()
                .await;

            match resp_result {
                Ok(r) => {
                    let status = r.status();
                    tracing::warn!(status = %status, "ChatGPT chat_completion_stream response received");
                    if !status.is_success() {
                        let text = r.text().await.unwrap_or_default();
                        last_error = format!("HTTP {}: (body_len={})", status, text.len());
                        tracing::warn!(status = %status, body = %text, "ChatGPT chat_completion_stream error response");
                        if status.as_u16() >= 400 && status.as_u16() < 500 && status.as_u16() != 429 {
                            anyhow::bail!("ChatGPT API error ({}): {}", status, text);
                        }
                        continue;
                    }
                    resp = Some(r);
                    break;
                }
                Err(e) => {
                    last_error = format!("request failed: {e}");
                    tracing::warn!(attempt, error = %e, "ChatGPT chat_completion_stream: network error");
                    continue;
                }
            }
        }

        let resp = resp.ok_or_else(|| anyhow::anyhow!("ChatGPT streaming request failed after {} attempts: {}", max_retries, last_error))?;
        let request_model = request.model.clone();
        let stream = resp.bytes_stream().map(move |chunk| {
            let chunk = chunk.context("stream chunk error")?;
            let text = String::from_utf8_lossy(&chunk);
            let mut last_delta: Option<ChatStreamDelta> = None;
            let mut current_event = String::new();
            for line in text.lines() {
                let line = line.trim();
                if let Some(ev) = line.strip_prefix("event:") {
                    current_event = ev.trim().to_string();
                    continue;
                }
                let data = match line.strip_prefix("data:") {
                    Some(d) => d.trim(),
                    None => continue,
                };
                if data == "[DONE]" {
                    current_event.clear();
                    continue;
                }
                let parsed: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => {
                        current_event.clear();
                        continue;
                    }
                };
                // Effective event type: prefer parsed["type"], fall back to current_event.
                let effective_event = parsed["type"].as_str().unwrap_or(&current_event);
                match effective_event {
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
                        let tool_calls: Vec<ToolCall> = parsed["response"]["output"]
                            .as_array()
                            .map(|output| {
                                output
                                    .iter()
                                    .filter_map(Self::tool_call_from_item)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let (finish_reason, tool_calls) = if tool_calls.is_empty() {
                            (FinishReason::Stop, None)
                        } else {
                            (FinishReason::ToolCalls, Some(tool_calls))
                        };
                        last_delta = Some(ChatStreamDelta {
                            id: String::new(),
                            model: request_model.clone(),
                            choices: vec![StreamChoice {
                                index: 0,
                                delta: DeltaMessage {
                                    role: None,
                                    content: Some(String::new()),
                                    function_call: None,
                                    tool_calls,
                                },
                                finish_reason: Some(finish_reason),
                            }],
                        });
                    }
                    _ => {}
                }
                current_event.clear();
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

    #[test]
    fn test_build_request_body_max_output_tokens() {
        // max_output_tokens must NOT appear in the request body (unsupported by the API).
        let provider = ChatGptProvider::new();
        let mut request = ChatRequest::new("gpt-5.5", vec![Message::user("hi")]);
        request.max_tokens = Some(256);
        let body = provider.build_request_body(&request, false);
        assert!(
            body.get("max_output_tokens").is_none(),
            "max_output_tokens must not be sent to the API"
        );

        let request_none = ChatRequest::new("gpt-5.5", vec![Message::user("hi")]);
        let body_none = provider.build_request_body(&request_none, false);
        assert!(body_none.get("max_output_tokens").is_none());
    }

    #[test]
    fn test_with_reasoning_effort_sets_max_output_tokens() {
        // The internal field is set but must NOT appear in the serialized body.
        let low = ChatGptProvider::new().with_reasoning_effort("low");
        let body_low = low
            .build_request_body(&ChatRequest::new("gpt-5.5", vec![Message::user("hi")]), false);
        assert!(
            body_low.get("max_output_tokens").is_none(),
            "max_output_tokens must not be sent to the API"
        );

        let high = ChatGptProvider::new().with_reasoning_effort("high");
        let body_high = high
            .build_request_body(&ChatRequest::new("gpt-5.5", vec![Message::user("hi")]), false);
        assert!(body_high.get("max_output_tokens").is_none());
    }

    #[test]
    fn test_parse_response_tool_calls() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",",
            "\"output\":[{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",",
            "\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"Tokyo\\\"}\"}],",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n",
            "\n",
        );
        let resp = provider.parse_response(sse, "gpt-5.5").expect("parse failed");
        assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::ToolCalls));
        let tool_calls = resp.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("expected tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].call_type, "function");
        assert_eq!(tool_calls[0].function.name, "get_weather");
        assert_eq!(tool_calls[0].function.arguments, "{\"city\":\"Tokyo\"}");

        // The content carries a <function_calls> XML block that the engine's
        // parse_xml_tool_calls() can recover.
        let content = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.clone(),
            _ => panic!("expected text content"),
        };
        assert!(content.contains("<function_calls>"), "content: {content}");
        assert!(content.contains("<invoke name=\"get_weather\">"), "content: {content}");
        assert!(content.contains("<city>Tokyo</city>"), "content: {content}");
    }

    // ── build_request_body field validation ──────────────────────────────────

    #[test]
    fn test_request_body_required_fields() {
        let provider = ChatGptProvider::new();
        let request = ChatRequest::new(
            "gpt-5.5",
            vec![
                Message::system("You are helpful."),
                Message::user("Hello"),
            ],
        );
        let body = provider.build_request_body(&request, false);

        // Required fields must be present.
        assert_eq!(body["model"], serde_json::json!("gpt-5.5"));
        assert!(body.get("input").is_some(), "input field must be present");
        assert_eq!(body["stream"], serde_json::json!(false));
        assert_eq!(body["store"], serde_json::json!(false));

        // max_output_tokens must NEVER appear (unsupported by Responses API).
        assert!(
            body.get("max_output_tokens").is_none(),
            "max_output_tokens must not be sent to the API"
        );
    }

    #[test]
    fn test_request_body_stream_flag() {
        let provider = ChatGptProvider::new();
        let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
        let body_stream = provider.build_request_body(&request, true);
        assert_eq!(body_stream["stream"], serde_json::json!(true));
        let body_no_stream = provider.build_request_body(&request, false);
        assert_eq!(body_no_stream["stream"], serde_json::json!(false));
    }

    #[test]
    fn test_request_body_reasoning_effort_low() {
        let provider = ChatGptProvider::new().with_reasoning_effort("low");
        let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
        let body = provider.build_request_body(&request, false);
        assert_eq!(
            body["reasoning"]["effort"],
            serde_json::json!("low"),
            "reasoning.effort must be 'low'"
        );
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn test_request_body_reasoning_effort_high() {
        let provider = ChatGptProvider::new().with_reasoning_effort("high");
        let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
        let body = provider.build_request_body(&request, false);
        assert_eq!(
            body["reasoning"]["effort"],
            serde_json::json!("high"),
            "reasoning.effort must be 'high'"
        );
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn test_request_body_no_reasoning_by_default() {
        // Default new() sets reasoning_effort = Some("low"), so "reasoning" WILL appear.
        // But if we explicitly clear it, it must not appear.
        let mut provider = ChatGptProvider::new();
        provider.reasoning_effort = None;
        let request = ChatRequest::new("gpt-5.5", vec![Message::user("Hi")]);
        let body = provider.build_request_body(&request, false);
        assert!(
            body.get("reasoning").is_none(),
            "reasoning field must not appear when reasoning_effort is None"
        );
    }

    #[test]
    fn test_request_body_instructions_from_system_message() {
        let provider = ChatGptProvider::new();
        let request = ChatRequest::new(
            "gpt-5.5",
            vec![
                Message::system("Be concise."),
                Message::user("Hi"),
            ],
        );
        let body = provider.build_request_body(&request, false);
        let instructions = body["instructions"].as_str().expect("instructions must be set");
        assert!(
            instructions.contains("Be concise."),
            "instructions must include system message"
        );
    }

    // ── parse_response delta text extraction ─────────────────────────────────

    #[test]
    fn test_parse_response_text_delta_single() {
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello, world!\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"output\":[],",
            "\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"total_tokens\":8}}}\n",
            "\n",
        );
        let resp = provider.parse_response(sse, "gpt-5.5").expect("parse failed");
        let text = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.as_str(),
            other => panic!("expected Text content, got: {other:?}"),
        };
        assert_eq!(text, "Hello, world!");
        assert_eq!(resp.usage.completion_tokens, 3);
    }

    #[test]
    fn test_parse_response_text_delta_multiple() {
        let provider = ChatGptProvider::new();
        // Multiple delta chunks must be concatenated in order.
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Foo\"}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\" \"}\n",
            "\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Bar\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"output\":[],",
            "\"usage\":{\"input_tokens\":2,\"output_tokens\":2,\"total_tokens\":4}}}\n",
            "\n",
        );
        let resp = provider.parse_response(sse, "gpt-5.5").expect("parse failed");
        let text = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.as_str(),
            other => panic!("expected Text content, got: {other:?}"),
        };
        assert_eq!(text, "Foo Bar");
    }

    #[test]
    fn test_parse_response_empty_no_output() {
        // A response with no delta events and no tool calls → empty content is fine.
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r3\",\"output\":[],",
            "\"usage\":{\"input_tokens\":1,\"output_tokens\":0,\"total_tokens\":1}}}\n",
            "\n",
        );
        let resp = provider.parse_response(sse, "gpt-5.5").expect("parse failed");
        assert_eq!(resp.usage.completion_tokens, 0);
        assert_eq!(resp.choices[0].finish_reason, Some(FinishReason::Stop));
    }

    #[test]
    fn test_parse_response_unicode_delta() {
        // Multibyte characters must be handled correctly.
        let provider = ChatGptProvider::new();
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"こんにちは\"}\n",
            "\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r4\",\"output\":[],",
            "\"usage\":{\"input_tokens\":1,\"output_tokens\":5,\"total_tokens\":6}}}\n",
            "\n",
        );
        let resp = provider.parse_response(sse, "gpt-5.5").expect("parse failed");
        let text = match &resp.choices[0].message.content {
            Some(MessageContent::Text(t)) => t.as_str(),
            other => panic!("expected Text content, got: {other:?}"),
        };
        assert_eq!(text, "こんにちは");
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
