use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const GEMINI_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Google Gemini API provider.
#[derive(Debug, Clone)]
pub struct GoogleProvider {
    client: Client,
    api_key: String,
    base_url: String,
}

impl GoogleProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: GEMINI_API_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Build the Gemini API URL for a given model and method.
    fn endpoint_url(&self, model: &str, method: &str) -> String {
        format!(
            "{}/models/{}:{}?key={}",
            self.base_url, model, method, self.api_key
        )
    }

    /// Convert unified messages to Gemini API format.
    fn build_request_body(&self, request: &ChatRequest) -> Value {
        let mut system_instruction: Option<Value> = None;
        let mut contents: Vec<Value> = Vec::new();

        for msg in &request.messages {
            match msg.role {
                Role::System => {
                    if let Some(text) = msg.text_content() {
                        system_instruction = Some(serde_json::json!({
                            "parts": [{"text": text}]
                        }));
                    }
                }
                Role::User => {
                    let parts = self.convert_parts(msg);
                    contents.push(serde_json::json!({
                        "role": "user",
                        "parts": parts,
                    }));
                }
                Role::Assistant => {
                    let parts = self.convert_parts(msg);
                    contents.push(serde_json::json!({
                        "role": "model",
                        "parts": parts,
                    }));
                }
                Role::Tool => {
                    // Gemini uses functionResponse parts
                    let name = msg.name.as_deref().unwrap_or("tool");
                    let text = msg.text_content().unwrap_or("{}");
                    let response_value: Value =
                        serde_json::from_str(text).unwrap_or(serde_json::json!({"result": text}));
                    contents.push(serde_json::json!({
                        "role": "function",
                        "parts": [{
                            "functionResponse": {
                                "name": name,
                                "response": response_value,
                            }
                        }],
                    }));
                }
            }
        }

        let mut body = serde_json::json!({
            "contents": contents,
        });

        if let Some(si) = system_instruction {
            body["systemInstruction"] = si;
        }

        // Generation config
        let mut gen_config = serde_json::json!({});
        if let Some(temp) = request.temperature {
            gen_config["temperature"] = serde_json::json!(temp);
        }
        if let Some(max) = request.max_tokens {
            gen_config["maxOutputTokens"] = serde_json::json!(max);
        }
        if let Some(ref stop) = request.stop {
            gen_config["stopSequences"] = serde_json::json!(stop);
        }
        if gen_config.as_object().map_or(false, |o| !o.is_empty()) {
            body["generationConfig"] = gen_config;
        }

        // Tools (function declarations)
        if let Some(ref functions) = request.functions {
            let declarations: Vec<Value> = functions
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "name": f.name,
                        "description": f.description,
                        "parameters": f.parameters,
                    })
                })
                .collect();
            body["tools"] = serde_json::json!([{
                "functionDeclarations": declarations,
            }]);
        }

        body
    }

    fn convert_parts(&self, msg: &Message) -> Vec<Value> {
        match &msg.content {
            Some(MessageContent::Text(text)) => {
                vec![serde_json::json!({"text": text})]
            }
            Some(MessageContent::Image { image_url, .. }) => {
                vec![serde_json::json!({
                    "inlineData": {
                        "mimeType": "image/jpeg",
                        "data": image_url.url,
                    }
                })]
            }
            Some(MessageContent::Multi(parts)) => {
                parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => serde_json::json!({"text": text}),
                        ContentPart::ImageUrl { image_url } => {
                            serde_json::json!({
                                "inlineData": {
                                    "mimeType": "image/jpeg",
                                    "data": image_url.url,
                                }
                            })
                        }
                    })
                    .collect()
            }
            None => vec![serde_json::json!({"text": ""})],
        }
    }

    /// Parse Gemini API response into unified format.
    fn parse_response(&self, body: Value, model: &str) -> Result<ChatResponse> {
        let candidates = body["candidates"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let mut choices: Vec<Choice> = Vec::new();
        for (i, candidate) in candidates.iter().enumerate() {
            let parts = candidate["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default();

            let mut text_parts: Vec<String> = Vec::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();

            for part in &parts {
                if let Some(text) = part["text"].as_str() {
                    text_parts.push(text.to_string());
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc["name"].as_str().unwrap_or_default().to_string();
                    let arguments = fc["args"].to_string();
                    tool_calls.push(ToolCall {
                        id: uuid::Uuid::new_v4().to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall { name, arguments },
                    });
                }
            }

            let content = if text_parts.is_empty() {
                None
            } else {
                Some(MessageContent::Text(text_parts.join("")))
            };

            let finish_reason = match candidate["finishReason"].as_str() {
                Some("STOP") => Some(FinishReason::Stop),
                Some("MAX_TOKENS") => Some(FinishReason::Length),
                Some("SAFETY") => Some(FinishReason::ContentFilter),
                _ => None,
            };

            choices.push(Choice {
                index: i as u32,
                message: Message {
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
                },
                finish_reason,
            });
        }

        let usage = if let Some(meta) = body.get("usageMetadata") {
            Usage {
                prompt_tokens: meta["promptTokenCount"].as_u64().unwrap_or(0) as u32,
                completion_tokens: meta["candidatesTokenCount"].as_u64().unwrap_or(0) as u32,
                total_tokens: meta["totalTokenCount"].as_u64().unwrap_or(0) as u32,
            }
        } else {
            Usage::default()
        };

        Ok(ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: model.to_string(),
            choices,
            usage,
            created: chrono::Utc::now().timestamp(),
        })
    }
}

#[async_trait]
impl LlmProvider for GoogleProvider {
    fn name(&self) -> &str {
        "google"
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/models?key={}", self.base_url, self.api_key);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to list Gemini models")?;

        let body: Value = resp.json().await.context("failed to parse model list")?;
        let models = body["models"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let name = m["name"].as_str()?;
                        // Strip "models/" prefix
                        let id = name.strip_prefix("models/").unwrap_or(name).to_string();
                        let display = m["displayName"].as_str().unwrap_or(&id).to_string();
                        let ctx = m["inputTokenLimit"].as_u64().unwrap_or(32_000) as u32;
                        Some(ModelInfo {
                            id,
                            name: display,
                            context_window: ctx,
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
        debug!(model = %request.model, "Google Gemini chat completion");

        let url = self.endpoint_url(&request.model, "generateContent");
        let body = self.build_request_body(&request);

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Gemini API request failed")?;

        let status = resp.status();
        let resp_body: Value = resp.json().await.context("failed to parse Gemini response")?;

        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            anyhow::bail!("Gemini API error ({}): {}", status, error_msg);
        }

        self.parse_response(resp_body, &request.model)
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "Google Gemini streaming chat completion");

        let url = self.endpoint_url(&request.model, "streamGenerateContent");
        let body = self.build_request_body(&request);

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Gemini streaming request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let err_body: Value = resp.json().await.unwrap_or_default();
            let msg = err_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            anyhow::bail!("Gemini API error ({}): {}", status, msg);
        }

        let model = request.model.clone();
        let stream = resp.bytes_stream().map(move |chunk| {
            let chunk = chunk.context("stream chunk error")?;
            let text = String::from_utf8_lossy(&chunk);
            let model = model.clone();

            // Gemini stream returns JSON array elements separated by commas
            let trimmed = text.trim().trim_start_matches('[').trim_end_matches(']').trim_start_matches(',').trim();

            let mut content_text = String::new();
            if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                if let Some(parts) = parsed["candidates"][0]["content"]["parts"].as_array() {
                    for part in parts {
                        if let Some(t) = part["text"].as_str() {
                            content_text.push_str(t);
                        }
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
        let url = format!("{}/models?key={}", self.base_url, self.api_key);
        let resp = self.client.get(&url).send().await?;
        Ok(resp.status().is_success())
    }
}
