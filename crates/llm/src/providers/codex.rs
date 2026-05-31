use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const DEFAULT_CODEX_PATH: &str = "codex";
const DEFAULT_MODEL: &str = "o4-mini";
const DEFAULT_TIMEOUT_SECS: u64 = 300;

static DEFAULT_MODELS: &[(&str, u32)] = &[
    ("o4-mini", 200_000),
    ("o3", 200_000),
    ("codex-mini", 200_000),
];

#[derive(Debug, Clone)]
pub struct CodexProvider {
    codex_path: String,
    default_model: String,
    sandbox: String,
    working_dir: Option<String>,
    timeout: Duration,
    extra_models: Vec<(String, u32)>,
}

impl CodexProvider {
    pub fn new() -> Self {
        Self {
            codex_path: DEFAULT_CODEX_PATH.to_string(),
            default_model: DEFAULT_MODEL.to_string(),
            sandbox: "read-only".to_string(),
            working_dir: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            extra_models: Vec::new(),
        }
    }

    pub fn with_codex_path(mut self, path: impl Into<String>) -> Self {
        self.codex_path = path.into();
        self
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    pub fn with_sandbox(mut self, sandbox: impl Into<String>) -> Self {
        self.sandbox = sandbox.into();
        self
    }

    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    /// Add additional model IDs beyond the built-in defaults.
    pub fn with_extra_models(mut self, models: Vec<(String, u32)>) -> Self {
        self.extra_models = models;
        self
    }

    fn build_prompt(&self, request: &ChatRequest) -> String {
        let mut parts = Vec::new();

        // Inject available tool definitions as an XML block so the model can
        // emit <function_calls> blocks (codex CLI has no native tool calling).
        if let Some(tools) = &request.functions {
            if !tools.is_empty() {
                parts.push(render_tool_definitions(tools));
            }
        }

        for msg in &request.messages {
            let content = msg.text_content().unwrap_or("");
            match msg.role {
                Role::System => {
                    if !content.is_empty() {
                        parts.push(format!("[System]\n{content}"));
                    }
                }
                Role::User => {
                    if !content.is_empty() {
                        parts.push(format!("[User]\n{content}"));
                    }
                }
                Role::Assistant => {
                    // An assistant turn may carry text, tool calls, or both.
                    let mut body = String::new();
                    if !content.is_empty() {
                        body.push_str(content);
                    }
                    if let Some(tool_calls) = &msg.tool_calls {
                        if !tool_calls.is_empty() {
                            if !body.is_empty() {
                                body.push_str("\n\n");
                            }
                            body.push_str(&render_tool_calls(tool_calls));
                        }
                    }
                    if !body.is_empty() {
                        parts.push(format!("[Assistant]\n{body}"));
                    }
                }
                Role::Tool => {
                    // Identify which tool call this result corresponds to so the
                    // model can match results back to its <function_calls>.
                    let name = msg.name.as_deref().unwrap_or("tool");
                    let header = match msg.tool_call_id.as_deref() {
                        Some(id) => format!("[Tool Result: {name} (call_id={id})]"),
                        None => format!("[Tool Result: {name}]"),
                    };
                    parts.push(format!("{header}\n{content}"));
                }
            }
        }

        parts.join("\n\n")
    }

    fn build_base_command(&self, model: &str, working_dir_override: Option<&str>) -> Command {
        let mut cmd = Command::new(&self.codex_path);
        cmd.arg("exec")
            .arg("--ephemeral")
            .arg("--skip-git-repo-check")
            .arg("-m")
            .arg(model)
            .arg("-s")
            .arg(&self.sandbox)
            // Never prompt for approval (non-interactive).
            .arg("-c")
            .arg("approval=never");

        // working_dir_override takes precedence over the configured working_dir.
        let working_dir = working_dir_override.or(self.working_dir.as_deref());
        if let Some(dir) = working_dir {
            cmd.arg("-C").arg(dir);
        }

        cmd
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmProvider for CodexProvider {
    fn name(&self) -> &str {
        "codex"
    }

    async fn available_models(&self) -> Result<Vec<ModelInfo>> {
        let mut models: Vec<ModelInfo> = DEFAULT_MODELS
            .iter()
            .map(|(id, ctx)| ModelInfo {
                id: id.to_string(),
                name: id.to_string(),
                context_window: *ctx,
                supports_function_calling: false,
                supports_vision: false,
            })
            .collect();

        for (id, ctx) in &self.extra_models {
            if !models.iter().any(|m| m.id == *id) {
                models.push(ModelInfo {
                    id: id.clone(),
                    name: id.clone(),
                    context_window: *ctx,
                    supports_function_calling: false,
                    supports_vision: false,
                });
            }
        }

        Ok(models)
    }

    async fn chat_completion(&self, request: ChatRequest) -> Result<ChatResponse> {
        let model = if request.model.is_empty() {
            &self.default_model
        } else {
            &request.model
        };
        debug!(model = %model, "Codex CLI chat completion");

        let working_dir_override = request
            .metadata
            .get("working_dir")
            .and_then(|v| v.as_str());

        let prompt = self.build_prompt(&request);

        // tempfile crate creates with O_EXCL and cleans up on Drop.
        let output_file = tempfile::Builder::new()
            .prefix("opencrab-codex-")
            .suffix(".txt")
            .tempfile()
            .context("failed to create temp file for codex output")?;
        let output_path = output_file.path().to_string_lossy().to_string();

        let mut cmd = self.build_base_command(model, working_dir_override);
        cmd.arg("-o")
            .arg(&output_path)
            // Read prompt from stdin to avoid ARG_MAX limits and /proc exposure.
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().context("failed to spawn codex CLI")?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .context("failed to write prompt to codex stdin")?;
        }

        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "codex CLI timed out after {}s",
                    self.timeout.as_secs()
                )
            })?
            .context("failed to wait for codex CLI")?;

        // Read response from the output file (-o flag).
        let response_text = match tokio::fs::read_to_string(&output_path).await {
            Ok(text) => {
                if !output.status.success() {
                    anyhow::bail!(
                        "codex exited with {} despite producing output",
                        output.status
                    );
                }
                text
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !output.status.success() {
                    anyhow::bail!(
                        "codex exec failed (exit {}): {}{}",
                        output.status,
                        stderr,
                        stdout
                    );
                }
                stdout.to_string()
            }
            Err(e) => {
                return Err(e).context("failed to read codex output file");
            }
        };
        // output_file (NamedTempFile) is dropped here, auto-deleting the file.

        let content = if response_text.trim().is_empty() {
            None
        } else {
            Some(MessageContent::Text(response_text))
        };

        // Parse usage from stdout JSONL if available (turn.completed events).
        let usage = parse_usage_from_stdout(&output.stdout);

        Ok(ChatResponse {
            id: uuid::Uuid::new_v4().to_string(),
            model: model.to_string(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content,
                    name: None,
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: None,
                    cache_control: None,
                },
                finish_reason: Some(FinishReason::Stop),
            }],
            usage,
            created: chrono::Utc::now().timestamp(),
        })
    }

    async fn chat_completion_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<ChatStreamDelta>>> {
        let model = if request.model.is_empty() {
            self.default_model.clone()
        } else {
            request.model.clone()
        };
        debug!(model = %model, "Codex CLI streaming chat completion");

        let working_dir_override = request
            .metadata
            .get("working_dir")
            .and_then(|v| v.as_str());

        let prompt = self.build_prompt(&request);

        let mut cmd = self.build_base_command(&model, working_dir_override);
        cmd.arg("--json")
            // Read prompt from stdin.
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = cmd.spawn().context("failed to spawn codex CLI for streaming")?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .context("failed to write prompt to codex stdin")?;
        }

        let stdout = child
            .stdout
            .take()
            .context("failed to get codex stdout")?;

        let reader = BufReader::new(stdout);
        let lines = reader.lines();
        let model_clone = model.clone();

        let stream = futures::stream::unfold(
            (lines, child, model_clone),
            |(mut lines, mut child, model)| async move {
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            if let Some(delta) = parse_jsonl_event(&line, &model) {
                                return Some((Ok(delta), (lines, child, model)));
                            }
                            // Non-content event, keep reading.
                        }
                        Ok(None) => {
                            // EOF — process finished.
                            let _ = child.wait().await;
                            return None;
                        }
                        Err(e) => {
                            let _ = child.wait().await;
                            return Some((
                                Err(anyhow::anyhow!("stream read error: {}", e)),
                                (lines, child, model),
                            ));
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }

    fn supports_function_calling(&self) -> bool {
        false
    }

    async fn health_check(&self) -> Result<bool> {
        let output = Command::new(&self.codex_path)
            .arg("--version")
            .output()
            .await;
        Ok(output.map(|o| o.status.success()).unwrap_or(false))
    }
}

/// Render tool definitions as an XML block describing how to invoke them.
/// codex CLI lacks native function calling, so we instruct the model to reply
/// with `<function_calls>` blocks that `parse_xml_tool_calls` understands.
fn render_tool_definitions(tools: &[FunctionDefinition]) -> String {
    let mut out = String::from(
        "[Available Tools]\nYou can call these tools by responding with \
         <function_calls>...</function_calls> XML blocks.\n\n<tools>\n",
    );

    for tool in tools {
        out.push_str(&format!("<tool name=\"{}\">\n", tool.name));
        if let Some(desc) = &tool.description {
            out.push_str(&format!("<description>{desc}</description>\n"));
        }
        // Embed the raw JSON Schema for the parameters.
        let params = serde_json::to_string(&tool.parameters)
            .unwrap_or_else(|_| "{}".to_string());
        out.push_str(&format!("<parameters>{params}</parameters>\n"));
        out.push_str("</tool>\n");
    }

    out.push_str(
        "</tools>\n\nTo call a tool, respond with:\n<function_calls>\n\
         <invoke name=\"tool_name\">\n<param_name>param_value</param_name>\n\
         </invoke>\n</function_calls>",
    );
    out
}

/// Render assistant tool calls back into `<function_calls>` XML so the model
/// sees its own prior calls in a format consistent with how it must produce them.
fn render_tool_calls(tool_calls: &[ToolCall]) -> String {
    let mut out = String::from("<function_calls>\n");

    for call in tool_calls {
        out.push_str(&format!("<invoke name=\"{}\">\n", call.function.name));
        // arguments is a JSON object string; expand each field into a param tag.
        match serde_json::from_str::<serde_json::Value>(&call.function.arguments) {
            Ok(serde_json::Value::Object(map)) => {
                for (key, value) in map {
                    let rendered = match value {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    };
                    out.push_str(&format!("<{key}>{rendered}</{key}>\n"));
                }
            }
            // Fall back to emitting the raw arguments if they are not an object.
            _ => {
                out.push_str(&format!("<arguments>{}</arguments>\n", call.function.arguments));
            }
        }
        out.push_str("</invoke>\n");
    }

    out.push_str("</function_calls>");
    out
}

/// Parse a JSONL event line from `codex exec --json` and extract streaming content.
/// Returns a ChatStreamDelta for agent_message item.completed events.
fn parse_jsonl_event(line: &str, model: &str) -> Option<ChatStreamDelta> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;

    let event_type = v.get("type")?.as_str()?;

    match event_type {
        "item.completed" => {
            let item = v.get("item")?;
            let item_type = item.get("type")?.as_str()?;
            if item_type == "agent_message" {
                let text = item.get("text")?.as_str()?;
                return Some(ChatStreamDelta {
                    id: uuid::Uuid::new_v4().to_string(),
                    model: model.to_string(),
                    choices: vec![StreamChoice {
                        index: 0,
                        delta: DeltaMessage {
                            role: Some(Role::Assistant),
                            content: Some(text.to_string()),
                            function_call: None,
                            tool_calls: None,
                        },
                        finish_reason: Some(FinishReason::Stop),
                    }],
                });
            }
            None
        }
        "turn.completed" => Some(ChatStreamDelta {
            id: uuid::Uuid::new_v4().to_string(),
            model: model.to_string(),
            choices: vec![StreamChoice {
                index: 0,
                delta: DeltaMessage {
                    role: None,
                    content: None,
                    function_call: None,
                    tool_calls: None,
                },
                finish_reason: Some(FinishReason::Stop),
            }],
        }),
        _ => None,
    }
}

/// Parse usage information from stdout JSONL (look for turn.completed with usage).
fn parse_usage_from_stdout(stdout: &[u8]) -> Usage {
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) == Some("turn.completed") {
                if let Some(usage) = v.get("usage") {
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let cached = usage
                        .get("cached_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let output = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    return Usage {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + output,
                        cache_read_input_tokens: cached,
                        cache_creation_input_tokens: 0,
                    };
                }
            }
        }
    }
    Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_prompt() {
        let provider = CodexProvider::new();
        let request = ChatRequest {
            model: "o4-mini".to_string(),
            messages: vec![
                Message {
                    role: Role::System,
                    content: Some(MessageContent::Text("You are helpful.".to_string())),
                    name: None,
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: None,
                    cache_control: None,
                },
                Message {
                    role: Role::User,
                    content: Some(MessageContent::Text("Hello".to_string())),
                    name: None,
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: None,
                    cache_control: None,
                },
            ],
            functions: None,
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: None,
            metadata: Default::default(),
        };

        let prompt = provider.build_prompt(&request);
        assert!(prompt.contains("[System]\nYou are helpful."));
        assert!(prompt.contains("[User]\nHello"));
    }

    #[test]
    fn test_build_prompt_injects_tool_definitions() {
        let provider = CodexProvider::new();
        let request = ChatRequest {
            model: "o4-mini".to_string(),
            messages: vec![Message::user("Do the thing")],
            functions: Some(vec![FunctionDefinition {
                name: "spawn_subtask".to_string(),
                description: Some("Launch a subtask asynchronously".to_string()),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"prompt": {"type": "string"}}
                }),
                cache_control: None,
            }]),
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: None,
            metadata: Default::default(),
        };

        let prompt = provider.build_prompt(&request);
        assert!(prompt.contains("[Available Tools]"));
        assert!(prompt.contains("<tool name=\"spawn_subtask\">"));
        assert!(prompt.contains("<description>Launch a subtask asynchronously</description>"));
        assert!(prompt.contains("\"prompt\""));
        assert!(prompt.contains("<function_calls>"));
        assert!(prompt.contains("[User]\nDo the thing"));
    }

    #[test]
    fn test_build_prompt_renders_assistant_tool_calls() {
        let provider = CodexProvider::new();
        let request = ChatRequest {
            model: "o4-mini".to_string(),
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: Some(MessageContent::Text("Calling a tool now.".to_string())),
                    name: None,
                    function_call: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: "send_message".to_string(),
                            arguments: r#"{"text":"hi","count":3}"#.to_string(),
                        },
                    }]),
                    tool_call_id: None,
                    cache_control: None,
                },
                Message {
                    role: Role::Tool,
                    content: Some(MessageContent::Text("done".to_string())),
                    name: Some("send_message".to_string()),
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: Some("call_1".to_string()),
                    cache_control: None,
                },
            ],
            functions: None,
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: None,
            metadata: Default::default(),
        };

        let prompt = provider.build_prompt(&request);
        // Assistant text and tool calls both rendered under [Assistant].
        assert!(prompt.contains("[Assistant]\nCalling a tool now."));
        assert!(prompt.contains("<invoke name=\"send_message\">"));
        assert!(prompt.contains("<text>hi</text>"));
        // Non-string JSON values are serialized without quotes.
        assert!(prompt.contains("<count>3</count>"));
        // Tool result identifies the originating call.
        assert!(prompt.contains("[Tool Result: send_message (call_id=call_1)]\ndone"));
    }

    #[test]
    fn test_default_values() {
        let provider = CodexProvider::new();
        assert_eq!(provider.codex_path, "codex");
        assert_eq!(provider.default_model, "o4-mini");
        assert_eq!(provider.sandbox, "read-only");
        assert!(provider.working_dir.is_none());
        assert_eq!(provider.timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_builder_methods() {
        let provider = CodexProvider::new()
            .with_codex_path("/usr/local/bin/codex")
            .with_default_model("o3")
            .with_sandbox("workspace-write")
            .with_working_dir("/home/user/project")
            .with_timeout_secs(600);

        assert_eq!(provider.codex_path, "/usr/local/bin/codex");
        assert_eq!(provider.default_model, "o3");
        assert_eq!(provider.sandbox, "workspace-write");
        assert_eq!(provider.working_dir.as_deref(), Some("/home/user/project"));
        assert_eq!(provider.timeout, Duration::from_secs(600));
    }

    #[test]
    fn test_extra_models() {
        let provider = CodexProvider::new()
            .with_extra_models(vec![("gpt-5".to_string(), 128_000)]);

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let models = rt.block_on(provider.available_models()).unwrap();
        assert!(models.iter().any(|m| m.id == "gpt-5"));
        assert!(models.iter().any(|m| m.id == "o4-mini"));
    }

    #[test]
    fn test_parse_jsonl_event_agent_message() {
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Hello world"}}"#;
        let delta = parse_jsonl_event(line, "o4-mini").unwrap();
        assert_eq!(delta.choices[0].delta.content.as_deref(), Some("Hello world"));
    }

    #[test]
    fn test_parse_jsonl_event_turn_completed() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":50,"output_tokens":20}}"#;
        let delta = parse_jsonl_event(line, "o4-mini").unwrap();
        assert_eq!(delta.choices[0].finish_reason, Some(FinishReason::Stop));
        assert!(delta.choices[0].delta.content.is_none());
    }

    #[test]
    fn test_parse_jsonl_event_irrelevant() {
        let line = r#"{"type":"thread.started","thread_id":"abc"}"#;
        assert!(parse_jsonl_event(line, "o4-mini").is_none());
    }

    #[test]
    fn test_parse_usage_from_stdout() {
        let stdout = br#"{"type":"thread.started","thread_id":"abc"}
{"type":"turn.started"}
{"type":"turn.completed","usage":{"input_tokens":500,"cached_input_tokens":400,"output_tokens":50}}
"#;
        let usage = parse_usage_from_stdout(stdout);
        assert_eq!(usage.prompt_tokens, 500);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 550);
        assert_eq!(usage.cache_read_input_tokens, 400);
    }
}
