use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;

use opencrab_core::{
    ChatMessage, ChatRequestSimple, ChatResponseSimple, LlmClient, ToolCall as CoreToolCall,
    ToolDefinition, UsageInfo,
};
use opencrab_llm::message::{
    ChatRequest, Choice, FinishReason, FunctionCall, FunctionDefinition, Message,
    MessageContent, Role, ToolCall as LlmToolCall, Usage,
};
use opencrab_llm::pricing::PricingRegistry;
use opencrab_llm::router::LlmRouter;

/// Configuration for metrics recording.
pub struct MetricsContext {
    pub db: Arc<Mutex<rusqlite::Connection>>,
    pub agent_id: String,
    pub session_id: Option<String>,
    pub pricing: PricingRegistry,
    /// Shared state: updated after each LLM call so actions can reference it.
    pub last_metrics_id: Arc<Mutex<Option<String>>>,
    /// Shared current purpose: actions (e.g. select_llm) can update this
    /// to tag subsequent LLM calls with the correct purpose.
    pub current_purpose: Arc<Mutex<String>>,
}

/// Adapter that wraps an `LlmRouter` and implements `LlmClient`
/// so that `SkillEngine` can use it directly.
///
/// Optionally records usage metrics to the DB after each call.
pub struct LlmRouterAdapter {
    router: Arc<LlmRouter>,
    metrics_ctx: Option<MetricsContext>,
}

impl LlmRouterAdapter {
    pub fn new(router: Arc<LlmRouter>) -> Self {
        Self {
            router,
            metrics_ctx: None,
        }
    }

    pub fn with_metrics(mut self, ctx: MetricsContext) -> Self {
        self.metrics_ctx = Some(ctx);
        self
    }
}

#[async_trait]
impl LlmClient for LlmRouterAdapter {
    async fn chat(&self, request: ChatRequestSimple) -> Result<ChatResponseSimple> {
        let model_requested = request.model.clone();
        let llm_request = to_llm_request(request);

        let start = std::time::Instant::now();
        let llm_response = self.router.chat_completion(llm_request).await?;
        let latency_ms = start.elapsed().as_millis() as i64;

        let response = from_llm_response(llm_response);

        // Record metrics to DB if context is available.
        if let Some(ref ctx) = self.metrics_ctx {
            let metrics_id = uuid::Uuid::new_v4().to_string();

            // Resolve provider and model from the alias.
            let (provider, model) = self
                .router
                .resolve_model(&model_requested)
                .unwrap_or_else(|_| ("unknown".to_string(), model_requested.clone()));

            let (input_tokens, output_tokens, total_tokens) = response
                .usage
                .as_ref()
                .map(|u| (u.prompt_tokens as i32, u.completion_tokens as i32, u.total_tokens as i32))
                .unwrap_or((0, 0, 0));

            let estimated_cost = ctx
                .pricing
                .calculate_cost(&provider, &model, input_tokens as u32, output_tokens as u32)
                .unwrap_or(0.0);

            let row = opencrab_db::queries::LlmMetricsRow {
                id: metrics_id.clone(),
                agent_id: ctx.agent_id.clone(),
                session_id: ctx.session_id.clone(),
                timestamp: Utc::now().to_rfc3339(),
                provider,
                model,
                purpose: ctx.current_purpose.lock().map(|p| p.clone()).unwrap_or_else(|_| "conversation".to_string()),
                task_type: None,
                complexity: None,
                input_tokens,
                output_tokens,
                total_tokens,
                estimated_cost_usd: estimated_cost,
                latency_ms,
                time_to_first_token_ms: None,
            };

            if let Ok(conn) = ctx.db.lock() {
                if let Err(e) = opencrab_db::queries::insert_llm_metrics(&conn, &row) {
                    tracing::warn!(error = %e, "Failed to record LLM metrics");
                }
            }

            // Update shared last_metrics_id so actions can reference it.
            if let Ok(mut id) = ctx.last_metrics_id.lock() {
                *id = Some(metrics_id);
            }
        }

        Ok(response)
    }
}

/// Convert core ChatRequestSimple → llm ChatRequest.
fn to_llm_request(req: ChatRequestSimple) -> ChatRequest {
    let messages: Vec<Message> = req.messages.into_iter().map(to_llm_message).collect();

    let functions: Option<Vec<FunctionDefinition>> = if req.tools.is_empty() {
        None
    } else {
        Some(req.tools.into_iter().map(to_function_def).collect())
    };

    ChatRequest {
        model: req.model,
        messages,
        functions,
        function_call: None,
        temperature: req.temperature.map(|t| t as f64),
        max_tokens: req.max_tokens,
        stop: None,
        stream: None,
        metadata: Default::default(),
    }
}

/// Convert a core ChatMessage → llm Message.
fn to_llm_message(msg: ChatMessage) -> Message {
    let role = match msg.role.as_str() {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    };

    let tool_calls = if msg.tool_calls.is_empty() {
        None
    } else {
        Some(
            msg.tool_calls
                .into_iter()
                .map(|tc| LlmToolCall {
                    id: tc.id,
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: tc.name,
                        arguments: serde_json::to_string(&tc.arguments)
                            .unwrap_or_else(|_| "{}".to_string()),
                    },
                })
                .collect(),
        )
    };

    Message {
        role,
        content: if msg.content.is_empty() {
            None
        } else {
            Some(MessageContent::Text(msg.content))
        },
        name: None,
        function_call: None,
        tool_calls,
        tool_call_id: msg.tool_call_id,
    }
}

/// Convert a core ToolDefinition → llm FunctionDefinition.
fn to_function_def(td: ToolDefinition) -> FunctionDefinition {
    FunctionDefinition {
        name: td.name,
        description: if td.description.is_empty() {
            None
        } else {
            Some(td.description)
        },
        parameters: td.parameters,
    }
}

/// Convert llm ChatResponse → core ChatResponseSimple.
fn from_llm_response(resp: opencrab_llm::message::ChatResponse) -> ChatResponseSimple {
    let first_choice: Option<&Choice> = resp.choices.first();

    let content = first_choice.and_then(|c| c.message.text_content().map(|s| s.to_string()));

    let tool_calls: Vec<CoreToolCall> = first_choice
        .and_then(|c| c.message.tool_calls.as_ref())
        .map(|tcs| {
            tcs.iter()
                .map(|tc| CoreToolCall {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                    arguments: serde_json::from_str(&tc.function.arguments)
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                })
                .collect()
        })
        .unwrap_or_default();

    let finish_reason = first_choice
        .and_then(|c| c.finish_reason.as_ref())
        .map(|fr| match fr {
            FinishReason::Stop => "stop".to_string(),
            FinishReason::Length => "length".to_string(),
            FinishReason::FunctionCall => "function_call".to_string(),
            FinishReason::ToolCalls => "tool_calls".to_string(),
            FinishReason::ContentFilter => "content_filter".to_string(),
        })
        .unwrap_or_else(|| "stop".to_string());

    let usage = Usage {
        prompt_tokens: resp.usage.prompt_tokens,
        completion_tokens: resp.usage.completion_tokens,
        total_tokens: resp.usage.total_tokens,
    };

    ChatResponseSimple {
        content,
        tool_calls,
        finish_reason,
        usage: Some(UsageInfo {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_llm_message_system() {
        let msg = ChatMessage {
            role: "system".to_string(),
            content: "You are helpful.".to_string(),
            tool_call_id: None,
            tool_calls: vec![],
        };
        let llm_msg = to_llm_message(msg);
        assert_eq!(llm_msg.role, Role::System);
        assert_eq!(llm_msg.text_content(), Some("You are helpful."));
    }

    #[test]
    fn test_to_llm_message_with_tool_calls() {
        let msg = ChatMessage {
            role: "assistant".to_string(),
            content: String::new(),
            tool_call_id: None,
            tool_calls: vec![CoreToolCall {
                id: "tc-1".to_string(),
                name: "search".to_string(),
                arguments: serde_json::json!({"query": "test"}),
            }],
        };
        let llm_msg = to_llm_message(msg);
        assert_eq!(llm_msg.role, Role::Assistant);
        let tcs = llm_msg.tool_calls.unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "search");
        assert_eq!(tcs[0].function.arguments, r#"{"query":"test"}"#);
    }

    #[test]
    fn test_from_llm_response_text() {
        let resp = opencrab_llm::message::ChatResponse {
            id: "r1".to_string(),
            model: "test".to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message::assistant("Hello!"),
                finish_reason: Some(FinishReason::Stop),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            },
            created: 0,
        };
        let simple = from_llm_response(resp);
        assert_eq!(simple.content, Some("Hello!".to_string()));
        assert!(simple.tool_calls.is_empty());
        assert_eq!(simple.finish_reason, "stop");
    }

    #[test]
    fn test_from_llm_response_tool_calls() {
        let mut msg = Message::assistant("");
        msg.content = None;
        msg.tool_calls = Some(vec![LlmToolCall {
            id: "tc-1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "learn".to_string(),
                arguments: r#"{"skill":"test"}"#.to_string(),
            },
        }]);

        let resp = opencrab_llm::message::ChatResponse {
            id: "r2".to_string(),
            model: "test".to_string(),
            choices: vec![Choice {
                index: 0,
                message: msg,
                finish_reason: Some(FinishReason::ToolCalls),
            }],
            usage: Usage::default(),
            created: 0,
        };
        let simple = from_llm_response(resp);
        assert!(simple.content.is_none());
        assert_eq!(simple.tool_calls.len(), 1);
        assert_eq!(simple.tool_calls[0].name, "learn");
        assert_eq!(simple.tool_calls[0].arguments["skill"], "test");
        assert_eq!(simple.finish_reason, "tool_calls");
    }
}
