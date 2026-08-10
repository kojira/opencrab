//! Canonical LLM message model shared by `opencrab-llm` (providers/router) and
//! `opencrab-core` (engine). This is a leaf crate with no dependencies beyond
//! serde, so both sibling crates can depend on it without creating a cycle.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Role of a message participant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Content of a message, supporting text, images, and multi-part content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Image {
        #[serde(rename = "type")]
        content_type: String,
        image_url: ImageUrl,
    },
    Multi(Vec<ContentPart>),
}

/// URL reference for an image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A single part of multi-part content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
}

/// A function call reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// A tool call from the assistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

impl ToolCall {
    /// Parse the (string) function arguments into a JSON value.
    ///
    /// Mirrors the historical adapter behaviour: malformed JSON degrades to an
    /// empty object rather than erroring, so downstream tool dispatch always
    /// receives a valid `Value`.
    pub fn arguments_json(&self) -> Value {
        serde_json::from_str(&self.function.arguments)
            .unwrap_or_else(|_| Value::Object(Default::default()))
    }
}

/// A single message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// For tool role messages, the tool_call_id this is responding to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// Create a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(MessageContent::Text(content.into())),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Create a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(MessageContent::Text(content.into())),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Create an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(MessageContent::Text(content.into())),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Create a tool response message.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(MessageContent::Text(content.into())),
            name: None,
            function_call: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    /// Extract text content if this message has simple text.
    pub fn text_content(&self) -> Option<&str> {
        match &self.content {
            Some(MessageContent::Text(s)) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// Definition of a callable function/tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

/// Controls how the model calls functions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FunctionCallBehavior {
    /// "auto" or "none"
    Mode(String),
    /// Force a specific function
    Named { name: String },
}

/// Request for a chat completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions: Option<Vec<FunctionDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCallBehavior>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Arbitrary metadata for provider-specific extensions.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, Value>,
    /// Identity of the agent making this request. Providers can derive
    /// agent-specific context (e.g. a workspace path) from this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Per-request reasoning (thinking) effort override ("minimal"|"low"|
    /// "medium"|"high"|"xhigh"). Providers prefer this over their
    /// construction-time value. None = provider/model default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl ChatRequest {
    /// Create a simple chat request with a model and messages.
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            functions: None,
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: None,
            metadata: HashMap::new(),
            agent_id: None,
            reasoning_effort: None,
        }
    }

    /// Set temperature.
    pub fn with_temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Set max tokens.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }
}

/// Token usage information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
}

/// Reason the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    FunctionCall,
    ToolCalls,
    ContentFilter,
}

/// A single completion choice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}

/// Response from a chat completion request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
    /// Timestamp of the response.
    pub created: i64,
}

impl ChatResponse {
    /// Get the text content of the first choice, if any.
    pub fn first_text(&self) -> Option<&str> {
        self.choices.first().and_then(|c| c.message.text_content())
    }

    /// Get the first choice's message.
    pub fn first_message(&self) -> Option<&Message> {
        self.choices.first().map(|c| &c.message)
    }

    /// Build a single-choice text response (convenience for tests and mocks).
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant(content),
                finish_reason: Some(FinishReason::Stop),
            }],
            usage: Usage::default(),
            created: 0,
        }
    }

    /// Build a single-choice response carrying tool calls and no text content
    /// (convenience for tests and mocks).
    pub fn with_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            id: String::new(),
            model: String::new(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: None,
                    name: None,
                    function_call: None,
                    tool_calls: Some(tool_calls),
                    tool_call_id: None,
                },
                finish_reason: Some(FinishReason::ToolCalls),
            }],
            usage: Usage::default(),
            created: 0,
        }
    }
}

/// A single chunk in a streaming response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatStreamDelta {
    pub id: String,
    pub model: String,
    pub choices: Vec<StreamChoice>,
}

/// A choice within a streaming chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChoice {
    pub index: u32,
    pub delta: DeltaMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}

/// Incremental message content in a stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// LLM プロバイダの型付きエラー（#35）。
///
/// リトライ/フォールバック方針や「このエラーは恒久的か」の判断が Display 文字列の
/// 部分一致に依存しないよう、HTTP ステータスを型として運ぶ。leaf crate に置くことで
/// llm（router）と core（daily_log_indexer 等）の両方が downcast できる。
/// anyhow は context チェーンを遡って downcast する。
#[derive(Debug)]
pub enum LlmError {
    /// HTTP ステータス付きの API エラー。
    Http {
        provider: &'static str,
        status: u16,
        message: String,
    },
}

impl LlmError {
    /// HTTP ステータス（あれば）。
    pub fn status(&self) -> Option<u16> {
        match self {
            LlmError::Http { status, .. } => Some(*status),
        }
    }

    /// リトライしても無駄な恒久エラー（429 以外の 4xx）か。
    /// ステータス不明のエラー種別が増えた場合は false（retryable）に倒す。
    pub fn is_non_retryable(&self) -> bool {
        match self.status() {
            Some(status) => status != 429 && (400..500).contains(&status),
            None => false,
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Http {
                provider,
                status,
                message,
            } => write!(f, "{provider} API error ({status}): {message}"),
        }
    }
}

impl std::error::Error for LlmError {}

/// `llm_logs.error_code` に入れる、context ウィンドウ超過を表す専用コード（#539）。
///
/// 従来は全エラーが総称 `"error"` で、context 超過（本番で 07-25〜08-10 に 54 件）を
/// ダッシュボードやアラートから判別できず、`error_body` の文字列一致に頼るしかなかった。
/// この定数を唯一の識別子として使う（読み手はこの値で分岐してよい）。
pub const CONTEXT_WINDOW_EXCEEDED_ERROR_CODE: &str = "context_window_exceeded";

/// context ウィンドウ超過（入力がモデル/経路の上限を超えて拒否された）エラーか（#539）。
///
/// **プロバイダ固有の拒否文言の一致はここ 1 箇所に集約する。** 他所（error_code の付与・
/// アラート・リトライ判定など）でこの判定を再実装しない。判定材料は provider がそのまま
/// 返す message（router が「All providers failed … [provider] <message>」へ集約した後の
/// 文字列でも部分一致で拾える）。HTTP 400 は他の bad request とも共有で status だけでは
/// 特定できないため、文言一致が実用上の唯一の軸。
///
/// マーカーは小文字で保持し、message も小文字化して部分一致で照合する。
/// - `"exceeds the context window"`: chatgpt（Codex OAuth 経路）。**本番 54 件で検証済み**。
/// - 残りは各社の既知文言だが**本番データには出現しておらず未検証**。誤検出を避けるため、
///   token/context 超過に十分固有な文言だけを保守的に並べる（超過でないものを超過と
///   誤判定する方が害が大きい: 気付けるはずのものが別名で埋もれる）。
///
/// **重複メモ（#539）**: context 関連の文字列リストは現在 2 箇所ある — ここ（超過の特定）と
/// `opencrab_core::memory::daily_log_indexer` の `NON_RETRYABLE_PATTERNS`（リトライ可否の
/// 判定）。**読み手（目的）が別なので今は統合しない。** 2 つまでは許容、**3 つ目が現れたら
/// 統合のサイン**。
pub fn is_context_window_error(message: &str) -> bool {
    const MARKERS: &[&str] = &[
        "exceeds the context window", // chatgpt / OpenAI Responses（本番検証済み）
        "maximum context length",     // OpenAI classic ("maximum context length is N tokens")
        "context length exceeded",    // OpenAI 互換の一部
        "reduce the length of the messages", // 上記 OpenAI 文言の後段
        "prompt is too long",         // Anthropic ("prompt is too long: N tokens > M maximum")
        "exceeds the maximum number of tokens", // Google Gemini 系
    ];
    let lower = message.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constructors() {
        let sys = Message::system("x");
        assert_eq!(sys.role, Role::System);

        let usr = Message::user("y");
        assert_eq!(usr.role, Role::User);

        let asst = Message::assistant("z");
        assert_eq!(asst.role, Role::Assistant);

        let tool = Message::tool("id", "r");
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(tool.tool_call_id.as_deref(), Some("id"));
    }

    #[test]
    fn test_text_content() {
        let msg = Message::user("hello");
        assert_eq!(msg.text_content(), Some("hello"));
    }

    #[test]
    fn context_window_error_matches_real_chatgpt_body() {
        // 本番 `llm_logs.error_body` の実形（router が集約した文字列）。
        let real = "All providers failed for model 'gpt-5.6-sol' (1 tried):\n  \
                    [chatgpt] ChatGPT API error: Your input exceeds the context window of this model.";
        assert!(is_context_window_error(real));
        // 大小無視。
        assert!(is_context_window_error(
            "PROMPT IS TOO LONG: 210000 tokens > 200000 maximum"
        ));
    }

    #[test]
    fn context_window_error_does_not_match_unrelated_failures() {
        // fallback 枯渇（本番の非超過エラーの典型）を context 超過と誤判定しない。
        assert!(!is_context_window_error(
            "All providers failed for model 'gpt-5.5'. Tried: chatgpt + fallback chain [\"ollama\"]"
        ));
        assert!(!is_context_window_error(
            "ChatGPT API error: rate limit exceeded"
        ));
        assert!(!is_context_window_error("request timed out"));
        assert!(!is_context_window_error(""));
    }

    #[test]
    fn test_first_text() {
        let response = ChatResponse::text("the answer");
        assert_eq!(response.first_text(), Some("the answer"));
    }

    #[test]
    fn test_builder() {
        let msgs = vec![Message::user("hi")];
        let req = ChatRequest::new("model", msgs)
            .with_temperature(0.5)
            .with_max_tokens(100);

        assert_eq!(req.model, "model");
        assert_eq!(req.temperature, Some(0.5));
        assert_eq!(req.max_tokens, Some(100));
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn test_arguments_json_valid_and_invalid() {
        let tc = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "f".into(),
                arguments: r#"{"a":1}"#.into(),
            },
        };
        assert_eq!(tc.arguments_json()["a"], serde_json::json!(1));

        let bad = ToolCall {
            id: "2".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "f".into(),
                arguments: "not json".into(),
            },
        };
        assert_eq!(bad.arguments_json(), serde_json::json!({}));
    }

    #[test]
    fn test_with_tool_calls_helper() {
        let tc = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "f".into(),
                arguments: "{}".into(),
            },
        };
        let resp = ChatResponse::with_tool_calls(vec![tc]);
        assert!(resp.first_text().is_none());
        assert_eq!(
            resp.first_message()
                .unwrap()
                .tool_calls
                .as_ref()
                .unwrap()
                .len(),
            1
        );
    }
}
