use anyhow::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::message::{ChatRequest, ChatResponse, ChatStreamDelta};

/// Information about an available model.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context_window: u32,
    pub supports_function_calling: bool,
    pub supports_vision: bool,
}

/// Trait for LLM providers.
///
/// Each provider (OpenAI, Anthropic, Google, etc.) implements this trait
/// to enable unified access through the router.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Returns the display name of this provider (e.g., "openai", "anthropic").
    fn name(&self) -> &str;

    /// Returns the list of models available from this provider.
    async fn available_models(&self) -> Result<Vec<ModelInfo>>;

    /// Perform a chat completion request.
    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// Perform a streaming chat completion request.
    ///
    /// Returns a stream of deltas. The default implementation falls back to
    /// a non-streaming call and yields a single synthetic delta.
    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        let response = self.chat_completion(request).await?;

        let delta = ChatStreamDelta {
            id: response.id,
            model: response.model,
            choices: response
                .choices
                .into_iter()
                .map(|c| crate::message::StreamChoice {
                    index: c.index,
                    delta: crate::message::DeltaMessage {
                        role: Some(c.message.role),
                        content: c.message.content.map(|mc| match mc {
                            crate::message::MessageContent::Text(s) => s,
                            _ => String::new(),
                        }),
                        function_call: c.message.function_call,
                        tool_calls: c.message.tool_calls,
                    },
                    finish_reason: c.finish_reason,
                })
                .collect(),
        };

        Ok(Box::pin(futures::stream::once(async { Ok(delta) })))
    }

    /// Whether this provider supports function calling / tools.
    fn supports_function_calling(&self) -> bool {
        false
    }

    /// Whether this provider supports vision (image inputs).
    fn supports_vision(&self) -> bool {
        false
    }

    /// Check if the provider's API endpoint is reachable.
    async fn health_check(&self) -> Result<bool> {
        Ok(true)
    }
}
