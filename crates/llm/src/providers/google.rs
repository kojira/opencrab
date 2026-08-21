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
    /// テレメトリ用の表示名（既定は形式名 "google"）。ルーティングキーは
    /// router 登録時に別途決まる。
    name: String,
}

impl GoogleProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.into(),
            base_url: GEMINI_API_URL.to_string(),
            name: "google".to_string(),
        }
    }

    /// 表示名を上書きする（同じ形式の接続先を別名で登録するとき）。
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
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
        let mut system_texts: Vec<String> = Vec::new();
        let mut contents: Vec<Value> = Vec::new();

        for msg in &request.messages {
            match msg.role {
                Role::System => {
                    // 複数の system メッセージは連結する（上書きすると先行指示が失われる）。
                    if let Some(text) = msg.text_content() {
                        if !text.is_empty() {
                            system_texts.push(text.to_string());
                        }
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

        if !system_texts.is_empty() {
            body["systemInstruction"] = serde_json::json!({
                "parts": [{"text": system_texts.join("\n\n")}]
            });
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
        if gen_config.as_object().is_some_and(|o| !o.is_empty()) {
            body["generationConfig"] = gen_config;
        }

        // Tools (function declarations)
        let mut tools: Vec<Value> = Vec::new();
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
            tools.push(serde_json::json!({
                "functionDeclarations": declarations,
            }));
        }
        // 本文URL読取り（エージェント単位オプトイン）: プロンプト中の URL を自動取得
        // する url_context を有効化（HTML/JSON/画像/PDF 対応）。function calling との
        // 併用は Gemini 3 系以降で公式サポート。それ以前（1.x/2.x）は併用が 400 に
        // なりうるため、フラグONでも付与せずスキップする（エージェントを壊さない）。
        if request
            .metadata
            .get("web_search")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            if Self::url_context_supported_model(&request.model) {
                tools.push(serde_json::json!({"url_context": {}}));
            } else {
                debug!(
                    model = %request.model,
                    "url_context skipped: model predates Gemini 3 (tool combination unsupported)"
                );
            }
        }
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
        }

        body
    }

    /// url_context を function calling と併用できるモデルか。
    /// Gemini 1.x/2.x は併用非対応（400 になりうる）ため除外し、それ以外
    /// （gemini-3 以降・将来系列）は許可する（allow-list より前方互換）。
    fn url_context_supported_model(model: &str) -> bool {
        let m = model.to_ascii_lowercase();
        !(m.starts_with("gemini-1") || m.starts_with("gemini-2"))
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
            Some(MessageContent::Multi(parts)) => parts
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
                .collect(),
            None => vec![serde_json::json!({"text": ""})],
        }
    }

    /// Parse Gemini API response into unified format.
    fn parse_response(&self, body: Value, model: &str) -> Result<ChatResponse> {
        let candidates = body["candidates"].as_array().cloned().unwrap_or_default();

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
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
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
        &self.name
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
        let resp_body: Value = resp
            .json()
            .await
            .context("failed to parse Gemini response")?;

        if !status.is_success() {
            let error_msg = resp_body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            return Err(crate::error::api_error("Gemini", status, error_msg));
        }

        self.parse_response(resp_body, &request.model)
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        debug!(model = %request.model, "Google Gemini streaming chat completion");

        // `alt=sse` を付けると Gemini は SSE 形式（`data: {json}` 行）で返す。
        // これを指定しないと pretty-print された1つの巨大JSON配列がチャンク分割されて届き、
        // チャンク単体では有効なJSONにならず本文が取り出せない。
        let url = format!(
            "{}&alt=sse",
            self.endpoint_url(&request.model, "streamGenerateContent")
        );
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
            return Err(crate::error::api_error("Gemini", status, msg));
        }

        let model = request.model.clone();
        // チャンク境界を跨いでバッファし、SSEの `data:` 行ごとに1デルタを emit する。
        let stream =
            crate::providers::sse::line_stream(resp.bytes_stream()).filter_map(move |line_res| {
                let model = model.clone();
                let out = match line_res {
                    Err(e) => Some(Err(e)),
                    Ok(line) => {
                        let line = line.trim();
                        if let Some(data) = line.strip_prefix("data:") {
                            let data = data.trim();
                            match serde_json::from_str::<Value>(data) {
                                Ok(parsed) => {
                                    let mut content_text = String::new();
                                    if let Some(parts) =
                                        parsed["candidates"][0]["content"]["parts"].as_array()
                                    {
                                        for part in parts {
                                            if let Some(t) = part["text"].as_str() {
                                                content_text.push_str(t);
                                            }
                                        }
                                    }
                                    if content_text.is_empty() {
                                        None
                                    } else {
                                        Some(Ok(ChatStreamDelta {
                                            id: String::new(),
                                            model,
                                            choices: vec![StreamChoice {
                                                index: 0,
                                                delta: DeltaMessage {
                                                    role: None,
                                                    content: Some(content_text),
                                                    function_call: None,
                                                    tool_calls: None,
                                                },
                                                finish_reason: None,
                                            }],
                                        }))
                                    }
                                }
                                Err(_) => None,
                            }
                        } else {
                            None
                        }
                    }
                };
                futures::future::ready(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// metadata の web_search=true で url_context ツールが tools に載ること。
    /// 未設定なら載らない（function declarations のみ/無し）。
    #[test]
    fn test_build_request_body_url_context_tool() {
        let provider = GoogleProvider::new("test-key");

        let mut request = ChatRequest::new("gemini-3-pro", vec![Message::user("このURLを見て")]);
        request
            .metadata
            .insert("web_search".to_string(), serde_json::json!(true));
        request.functions = Some(vec![FunctionDefinition {
            name: "my_tool".to_string(),
            description: None,
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]);
        let body = provider.build_request_body(&request);
        let tools = body["tools"].as_array().expect("tools array");
        assert!(tools
            .iter()
            .any(|t| t.get("functionDeclarations").is_some()));
        assert!(
            tools.iter().any(|t| t.get("url_context").is_some()),
            "url_context tool present"
        );

        let plain = ChatRequest::new("gemini-3-pro", vec![Message::user("hi")]);
        let body = provider.build_request_body(&plain);
        assert!(body.get("tools").is_none());
    }

    /// Gemini 1.x/2.x は function calling との併用が 400 になりうるため、フラグON
    /// でも url_context を付与しない（エージェントを壊さない側に倒す）。
    #[test]
    fn test_url_context_skipped_on_pre_gemini3_models() {
        let provider = GoogleProvider::new("test-key");
        let mut request = ChatRequest::new("gemini-2.5-pro", vec![Message::user("URLを見て")]);
        request
            .metadata
            .insert("web_search".to_string(), serde_json::json!(true));
        request.functions = Some(vec![FunctionDefinition {
            name: "my_tool".to_string(),
            description: None,
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }]);
        let body = provider.build_request_body(&request);
        let tools = body["tools"].as_array().expect("tools array");
        assert!(
            !tools.iter().any(|t| t.get("url_context").is_some()),
            "url_context must be skipped on gemini-2.x"
        );

        assert!(GoogleProvider::url_context_supported_model("gemini-3-pro"));
        assert!(GoogleProvider::url_context_supported_model(
            "gemini-4-flash"
        ));
        assert!(!GoogleProvider::url_context_supported_model(
            "gemini-2.5-flash"
        ));
        assert!(!GoogleProvider::url_context_supported_model(
            "gemini-1.5-pro"
        ));
    }
}
