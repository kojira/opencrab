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
    /// テレメトリ用の表示名（既定は形式名 "ollama"）。ルーティングキーは
    /// router 登録時に別途決まる。
    name: String,
}

impl OllamaProvider {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            base_url: OLLAMA_DEFAULT_URL.to_string(),
            name: "ollama".to_string(),
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

        let tool_calls = msg
            .get("tool_calls")
            .and_then(|tc| serde_json::from_value::<Vec<ToolCall>>(tc.clone()).ok());

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
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            created: chrono::Utc::now().timestamp(),
        })
    }
}

/// Ollama の NDJSON 1行（`sse::line_stream` が yield した完全行）から delta を組み立てる。
/// 空行・パース不能な行は None（keep-alive 等）。
/// Ollama の mid-stream エラーオブジェクト（`{"error": "..."}` ）は Err として表面化させる
/// （握りつぶすと、生成が途中で落ちたのに finish_reason 無しでストリームが終わり、
/// 消費側が「正常完了した短い応答」と誤認する）。
fn delta_from_ndjson_line(line: &str, model: &str) -> Option<Result<ChatStreamDelta>> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let parsed = serde_json::from_str::<Value>(line).ok()?;
    if let Some(err) = parsed["error"].as_str() {
        return Some(Err(anyhow::anyhow!("Ollama stream error: {err}")));
    }
    let content = parsed["message"]["content"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(String::from);
    let done = parsed["done"].as_bool().unwrap_or(false);
    if content.is_none() && !done {
        return None;
    }
    Some(Ok(ChatStreamDelta {
        id: uuid::Uuid::new_v4().to_string(),
        model: model.to_string(),
        choices: vec![StreamChoice {
            index: 0,
            delta: DeltaMessage {
                role: None,
                content,
                function_call: None,
                tool_calls: None,
            },
            finish_reason: if done { Some(FinishReason::Stop) } else { None },
        }],
    }))
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to list Ollama models")?;

        let body: Value = resp
            .json()
            .await
            .context("failed to parse Ollama model list")?;
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
            return Err(crate::error::api_error("Ollama", status, err_text));
        }

        let resp_body: Value = resp
            .json()
            .await
            .context("failed to parse Ollama response")?;
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
            return Err(crate::error::api_error("Ollama", status, err_text));
        }

        // Ollama は NDJSON（1行=1 JSON オブジェクト）を流す。行分割は共有の
        // `sse::line_stream` に任せる — チャンク境界で JSON が分断されると黙って
        // 捨てられる / マルチバイト UTF-8 が per-chunk lossy デコードで壊れる、
        // という修正済みバグの再実装をしない（#38）。
        let model = request.model.clone();
        let stream =
            crate::providers::sse::line_stream(resp.bytes_stream()).filter_map(move |line_res| {
                let out = match line_res {
                    Err(e) => Some(Err(e)),
                    Ok(line) => delta_from_ndjson_line(&line, &model),
                };
                futures::future::ready(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn ndjson_line_variants() {
        let d = delta_from_ndjson_line(r#"{"message":{"content":"こん"},"done":false}"#, "m")
            .unwrap()
            .unwrap();
        assert_eq!(d.choices[0].delta.content.as_deref(), Some("こん"));
        assert_eq!(d.choices[0].finish_reason, None);

        // done 行（content 空でも emit され finish_reason が付く）
        let d = delta_from_ndjson_line(r#"{"message":{"content":""},"done":true}"#, "m")
            .unwrap()
            .unwrap();
        assert_eq!(d.choices[0].delta.content, None);
        assert_eq!(d.choices[0].finish_reason, Some(FinishReason::Stop));

        // 空行・壊れた JSON・情報の無い行は None
        assert!(delta_from_ndjson_line("", "m").is_none());
        assert!(delta_from_ndjson_line("{broken", "m").is_none());
        assert!(delta_from_ndjson_line(r#"{"noise":1}"#, "m").is_none());

        // mid-stream エラーオブジェクトは Err として表面化（黙って握りつぶさない）
        let err = delta_from_ndjson_line(r#"{"error":"model runner stopped"}"#, "m")
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("model runner stopped"));
    }

    /// チャンク境界で JSON オブジェクト・マルチバイト UTF-8 が分断されても
    /// content が欠落・破壊されないこと（#38 で再導入されていたバグの回帰テスト）。
    #[tokio::test]
    async fn ndjson_survives_chunk_boundaries() {
        // 「あ」(E3 81 82) の途中 + JSON オブジェクトの途中でチャンクを割る
        let full = "{\"message\":{\"content\":\"あい\"},\"done\":false}\n{\"message\":{\"content\":\"うえ\"},\"done\":true}\n";
        let bytes = full.as_bytes();
        let chunks: Vec<reqwest::Result<Vec<u8>>> = vec![
            Ok(bytes[..25].to_vec()),   // 「あ」のバイト途中
            Ok(bytes[25..50].to_vec()), // 1つ目のオブジェクト途中〜2つ目の先頭
            Ok(bytes[50..].to_vec()),
        ];
        let byte_stream = futures::stream::iter(chunks);
        let deltas: Vec<ChatStreamDelta> = crate::providers::sse::line_stream(byte_stream)
            .filter_map(|r| futures::future::ready(delta_from_ndjson_line(&r.unwrap(), "m")))
            .map(|r| r.unwrap())
            .collect()
            .await;

        let text: String = deltas
            .iter()
            .filter_map(|d| d.choices[0].delta.content.clone())
            .collect();
        assert_eq!(text, "あいうえ");
        assert_eq!(
            deltas.last().unwrap().choices[0].finish_reason,
            Some(FinishReason::Stop)
        );
    }
}
