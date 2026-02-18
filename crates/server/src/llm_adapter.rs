use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use opencrab_core::{
    ChatMessage, ChatRequestSimple, ChatResponseSimple, LlmClient, ToolCall as CoreToolCall,
    ToolDefinition, UsageInfo,
};
use opencrab_llm::message::{
    ChatRequest, Choice, FinishReason, FunctionCall, FunctionDefinition, Message,
    MessageContent, Role, ToolCall as LlmToolCall, Usage,
};
use opencrab_llm::router::LlmRouter;

/// Adapter that wraps an `LlmRouter` and implements `LlmClient`
/// so that `SkillEngine` can use it directly.
pub struct LlmRouterAdapter {
    router: Arc<LlmRouter>,
}

impl LlmRouterAdapter {
    pub fn new(router: Arc<LlmRouter>) -> Self {
        Self { router }
    }
}

#[async_trait]
impl LlmClient for LlmRouterAdapter {
    async fn chat(&self, request: ChatRequestSimple) -> Result<ChatResponseSimple> {
        let llm_request = to_llm_request(request);
        let llm_response = self.router.chat_completion(llm_request).await?;
        Ok(from_llm_response(llm_response))
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
