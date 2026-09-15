use super::*;
use async_trait::async_trait;
use opencrab_llm_types::{
    ChatRequest, ChatResponse, Choice, FunctionCall, FunctionDefinition, Message, MessageContent,
    Role, ToolCall, Usage,
};

struct ScriptedLlm(std::sync::Mutex<Vec<anyhow::Result<ChatResponse>>>);

#[async_trait]
impl LlmClient for ScriptedLlm {
    async fn chat(&self, _request: ChatRequest) -> anyhow::Result<ChatResponse> {
        self.0.lock().unwrap().remove(0)
    }
}

struct SuccessfulExecutor;

#[async_trait]
impl ActionExecutor for SuccessfulExecutor {
    async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ActionResult {
        ActionResult {
            success: true,
            data: serde_json::json!(null),
            error: None,
        }
    }

    fn list_tools(&self) -> Vec<FunctionDefinition> {
        ["test_tool", "reply"]
            .into_iter()
            .map(|name| FunctionDefinition {
                name: name.to_string(),
                description: None,
                parameters: serde_json::json!({}),
            })
            .collect()
    }
}

struct UtteranceDispatcher;

impl crate::ToolDispatcher for UtteranceDispatcher {
    fn should_dispatch(&self, name: &str) -> bool {
        name != "reply"
    }

    fn is_utterance(&self, name: &str) -> bool {
        name == "reply"
    }

    fn dispatch_batch(&self, _calls: &[crate::DispatchCall]) -> crate::DispatchOutcome {
        crate::DispatchOutcome {
            subtask_id: "subtask".into(),
            label: "test".into(),
        }
    }
}

fn tool_call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        call_type: "function".into(),
        function: FunctionCall {
            name: name.into(),
            arguments: "{}".into(),
        },
    }
}

fn response(text: Option<&str>, calls: Vec<ToolCall>) -> ChatResponse {
    ChatResponse {
        id: String::new(),
        model: String::new(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: text.map(|text| MessageContent::Text(text.into())),
                name: None,
                function_call: None,
                tool_calls: (!calls.is_empty()).then_some(calls),
                tool_call_id: None,
            },
            finish_reason: None,
        }],
        usage: Usage::default(),
        created: 0,
    }
}

fn engine(script: Vec<anyhow::Result<ChatResponse>>, limit: usize) -> SkillEngine {
    let mut engine = SkillEngine::new(
        Box::new(ScriptedLlm(std::sync::Mutex::new(script))),
        Box::new(SuccessfulExecutor),
        limit,
    );
    engine.set_initial_read_origin("origin-a".into());
    engine
}

#[tokio::test]
async fn successful_utterance_resolves_origin_as_non_silent() {
    let mut engine = engine(
        vec![Ok(response(
            Some("NO_REPLY"),
            vec![tool_call("reply-1", "reply")],
        ))],
        2,
    );
    engine.set_tool_dispatcher(std::sync::Arc::new(UtteranceDispatcher));

    let result = engine.run("system", "reply", "model").await.unwrap();

    assert!(result.silent_origins.is_empty());
    assert_eq!(result.last_posting_utterance_id.as_deref(), Some("reply-1"));
}

#[tokio::test]
async fn continuation_delivery_failure_produces_no_silent_outcome() {
    let mut engine = engine(
        vec![Ok(response(
            Some("visible status"),
            vec![tool_call("tool-1", "test_tool")],
        ))],
        2,
    );
    engine.set_on_continuation_speech(std::sync::Arc::new(|_| {
        Box::pin(async { anyhow::bail!("simulated delivery failure") })
    }));

    let error = engine.run("system", "work", "model").await.unwrap_err();

    assert!(error.to_string().contains("holding speech delivery failed"));
}

#[tokio::test]
async fn llm_error_produces_no_silent_outcome() {
    let engine = engine(vec![Err(anyhow::anyhow!("simulated llm error"))], 2);

    let error = engine.run("system", "work", "model").await.unwrap_err();

    assert!(error.to_string().contains("simulated llm error"));
}

#[tokio::test]
async fn iteration_limit_does_not_reclassify_pending_origin_as_silent() {
    let engine = engine(
        vec![Ok(response(None, vec![tool_call("tool-1", "test_tool")]))],
        1,
    );

    let result = engine.run("system", "work", "model").await.unwrap();

    assert!(result.stopped_by_limit);
    assert!(result.silent_origins.is_empty());
}
