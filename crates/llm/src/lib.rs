pub mod message;
pub mod metrics;
pub mod pricing;
pub mod providers;
pub mod router;
pub mod traits;

// Re-export primary types for convenience.
pub use message::{
    ChatRequest, ChatResponse, ChatStreamDelta, Choice, ContentPart, DeltaMessage, FinishReason,
    FunctionCall, FunctionCallBehavior, FunctionDefinition, ImageUrl, Message, MessageContent,
    Role, StreamChoice, ToolCall, Usage,
};
pub use metrics::MetricsCollector;
pub use pricing::{ModelPricing, PricingRegistry};
pub use router::LlmRouter;
pub use traits::{LlmProvider, ModelInfo};

// Re-export providers.
pub use providers::{
    AnthropicProvider, GoogleProvider, LlamaCppProvider, OllamaProvider, OpenAiProvider,
    OpenRouterProvider,
};
