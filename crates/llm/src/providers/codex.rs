use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const DEFAULT_CODEX_PATH: &str = "codex";
const DEFAULT_MODEL: &str = "o4-mini";
const DEFAULT_TIMEOUT_SECS: u64 = 300;

static DEFAULT_MODELS: &[(&str, u32)] = &[
    // GPT-5.6 family（Codex サブスクで `codex exec -m gpt-5.6` として利用可。
    // gpt-5.6 は Sol にエイリアス。コンテキストは 1,050,000）。
    ("gpt-5.6", 1_050_000),
    ("gpt-5.6-sol", 1_050_000),
    ("gpt-5.6-terra", 1_050_000),
    ("gpt-5.6-luna", 1_050_000),
    ("gpt-5.5", 400_000),
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
    /// `-c model_reasoning_effort=<low|medium|high|xhigh>` の上書き。
    /// 空/未設定なら送らずモデル既定に従う。gpt-5.6-sol は既定 high。
    reasoning_effort: Option<String>,
    /// テレメトリ用の表示名（既定は形式名 "codex"）。ルーティングキーは
    /// router 登録時に別途決まる。
    name: String,
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
            reasoning_effort: None,
            name: "codex".to_string(),
        }
    }

    /// 表示名を上書きする（同じ形式の接続先を別名で登録するとき）。
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_codex_path(mut self, path: impl Into<String>) -> Self {
        self.codex_path = path.into();
        self
    }

    /// codex の reasoning effort を上書きする（"low"|"medium"|"high"|"xhigh"）。
    /// 空文字は「未設定」として扱う。
    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        let s = effort.into();
        self.reasoning_effort = if s.trim().is_empty() { None } else { Some(s) };
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
        build_cli_prompt(request)
    }

    /// `effort` は per-request（エージェント個別）優先の実効 reasoning effort。
    fn build_base_command(
        &self,
        model: &str,
        agent_id: Option<&str>,
        effort: Option<&str>,
    ) -> Command {
        let mut cmd = Command::new(&self.codex_path);
        // タイムアウトやストリーム破棄で future が drop されたとき、codex プロセスを
        // 確実に kill する。これが無いと、タイムアウト後も codex が走り続けて
        // 孤児プロセスが蓄積する（workspace-write サンドボックスでの書き込みも継続する）。
        cmd.kill_on_drop(true);
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

        // reasoning effort の上書き（設定時のみ）。gpt-5.6-sol 等は既定 high で
        // 遅い/高コストなので、明示指定で下げ/上げできる。
        if let Some(effort) = effort {
            cmd.arg("-c")
                .arg(format!("model_reasoning_effort={effort}"));
        }

        // Derive a per-agent workspace path from the agent identity. Falls back
        // to the provider-configured working_dir when no agent_id is supplied.
        let working_dir: Option<String> = agent_id
            .map(|id| format!("data/agents/{}/workspace", id))
            .or_else(|| self.working_dir.clone());

        if let Some(dir) = &working_dir {
            std::fs::create_dir_all(dir).ok();
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
        &self.name
    }

    // #676: codex CLI 経由で max_tokens を送る口が無いため出力上限の登録は不要（opt-out）。
    fn sends_max_output_tokens(&self) -> bool {
        false
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

        let prompt = self.build_prompt(&request);

        // tempfile crate creates with O_EXCL and cleans up on Drop.
        let output_file = tempfile::Builder::new()
            .prefix("opencrab-codex-")
            .suffix(".txt")
            .tempfile()
            .context("failed to create temp file for codex output")?;
        let output_path = output_file.path().to_string_lossy().to_string();

        let effort = request
            .reasoning_effort
            .as_deref()
            .or(self.reasoning_effort.as_deref());
        let mut cmd = self.build_base_command(model, request.agent_id.as_deref(), effort);
        // `--json` で stdout に JSONL イベント（turn.completed 等）を出させ、usage を
        // 取得する（#148: 非ストリーミング経路は従来 --json 未指定で usage が全ゼロだった）。
        // 最終メッセージ本文の取得方法は変えない: 引き続き `-o` ファイルから読む
        // （--json と -o は直交する: --json は stdout 形式、-o は最終メッセージ書き出しで、
        // 併用しても -o ファイルは書かれる）。usage 取得のためだけに --json を併用する。
        append_nonstreaming_output_args(&mut cmd, &output_path);
        cmd.stdin(std::process::Stdio::piped())
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
            .map_err(|_| anyhow::anyhow!("codex CLI timed out after {}s", self.timeout.as_secs()))?
            .context("failed to wait for codex CLI")?;

        // Read the final message from the output file (-o flag). NotFound は None
        // に畳んで resolve_codex_output で判断する（それ以外の I/O エラーは即返す）。
        let file_text: Option<String> = match tokio::fs::read_to_string(&output_path).await {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("failed to read codex output file"),
        };
        let response_text = resolve_codex_output(
            &format!("{}", output.status),
            output.status.success(),
            file_text,
            &output.stdout,
            &output.stderr,
        )?;
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

        let prompt = self.build_prompt(&request);

        let effort = request
            .reasoning_effort
            .as_deref()
            .or(self.reasoning_effort.as_deref());
        let mut cmd = self.build_base_command(&model, request.agent_id.as_deref(), effort);
        cmd.arg("--json")
            // Read prompt from stdin.
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = cmd
            .spawn()
            .context("failed to spawn codex CLI for streaming")?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .context("failed to write prompt to codex stdin")?;
        }

        let stdout = child.stdout.take().context("failed to get codex stdout")?;

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

/// メッセージ本文を CLI プロンプト用テキストに落とす。
///
/// マルチパート（画像添付）メッセージでも Text 部分は必ず拾い、画像は「このモデルは
/// 直接読めない」旨を注記する。以前は `text_content()` を使っており、Multi/Image だと
/// `None` を返すため **本文ごと丸ごと落ちていた**（画像を貼ると発言が消える）。codex /
/// cursor は画像を直接扱えないが、少なくともユーザーの言葉と「画像がある」事実は
/// モデルに届ける（gpt-5.6 系で画像を読むなら chatgpt プロバイダ経由を使う）。
fn cli_message_text(msg: &Message) -> String {
    match &msg.content {
        Some(MessageContent::Text(s)) => s.clone(),
        Some(MessageContent::Multi(parts)) => {
            let mut text = String::new();
            let mut images = 0usize;
            for part in parts {
                match part {
                    ContentPart::Text { text: t } => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                    ContentPart::ImageUrl { .. } => images += 1,
                }
            }
            if images > 0 {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&format!(
                    "[注記: 画像が {images} 枚添付されていますが、このモデルは画像を\
                     直接読めません。必要なら送信者に内容の説明を求めてください。]"
                ));
            }
            text
        }
        Some(MessageContent::Image { .. }) => "[注記: 画像が添付されていますが、このモデルは\
             画像を直接読めません。必要なら送信者に内容の説明を求めてください。]"
            .to_string(),
        None => String::new(),
    }
}

/// CLI 系プロバイダ（codex / cursor）共通のプロンプト組み立て。
/// ネイティブ function calling が無いため、tool 定義を XML でプロンプトに載せ、
/// 会話履歴を `[System]`/`[User]`/`[Assistant]`/`[Tool Result]` で連結する。
pub(crate) fn build_cli_prompt(request: &ChatRequest) -> String {
    let mut parts = Vec::new();

    if let Some(tools) = &request.functions {
        if !tools.is_empty() {
            parts.push(render_tool_definitions(tools));
        }
    }

    for msg in &request.messages {
        let content_owned = cli_message_text(msg);
        let content = content_owned.as_str();
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

/// Render tool definitions as an XML block describing how to invoke them.
/// codex CLI lacks native function calling, so we instruct the model to reply
/// with `<function_calls>` blocks that `parse_xml_tool_calls` understands.
pub(crate) fn render_tool_definitions(tools: &[FunctionDefinition]) -> String {
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
        let params = serde_json::to_string(&tool.parameters).unwrap_or_else(|_| "{}".to_string());
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
pub(crate) fn render_tool_calls(tool_calls: &[ToolCall]) -> String {
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
                out.push_str(&format!(
                    "<arguments>{}</arguments>\n",
                    call.function.arguments
                ));
            }
        }
        out.push_str("</invoke>\n");
    }

    out.push_str("</function_calls>");
    out
}

/// codex の終了状態と出力から最終応答テキストを決める。
///
/// codex は最終メッセージ（`-o` ファイル）を書き出したのに非ゼロ終了することが
/// ある（gpt-5.6 系の agentic な後処理・サンドボックス動作の失敗など）。その場合
/// でも**応答本文が非空なら捨てずに使う**（総失敗 → フォールバックより有用）。
/// 原因究明のため exit code と stderr は warn に必ず残す（握りつぶさない）。
///
/// - `-o` ファイルあり（`file_text` = Some）:
///   - 非ゼロ終了 かつ 本文空 → 失敗（stderr 付きで返す）
///   - 非ゼロ終了 かつ 本文あり → warn を出して本文を採用
///   - 正常終了 → 本文を採用
/// - `-o` ファイル無し（`file_text` = None、古い codex 等）:
///   - 非ゼロ終了 → 失敗（stderr + stdout 付きで返す）
///   - 正常終了 → stdout を採用
fn resolve_codex_output(
    exit_display: &str,
    success: bool,
    file_text: Option<String>,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<String> {
    let stderr_s = String::from_utf8_lossy(stderr);
    match file_text {
        Some(text) => {
            if !success {
                if text.trim().is_empty() {
                    anyhow::bail!(
                        "codex exec failed (exit {}) with empty output: {}",
                        exit_display,
                        stderr_s.trim()
                    );
                }
                warn!(
                    exit = %exit_display,
                    stderr = %stderr_s.trim(),
                    "codex exited non-zero but produced a response; using the response"
                );
            }
            Ok(text)
        }
        None => {
            let stdout_s = String::from_utf8_lossy(stdout);
            if !success {
                anyhow::bail!(
                    "codex exec failed (exit {}): {}{}",
                    exit_display,
                    stderr_s,
                    stdout_s
                );
            }
            Ok(stdout_s.to_string())
        }
    }
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

/// Append the args for the non-streaming `chat_completion` codex invocation.
///
/// `--json` makes codex emit JSONL events (incl. `turn.completed` with usage)
/// on stdout so `parse_usage_from_stdout` can account tokens (#148: without it
/// usage was all-zero). `-o <path>` still writes the final message to the temp
/// file (orthogonal to `--json`), and a trailing `-` reads the prompt from stdin
/// to avoid ARG_MAX limits / `/proc` exposure. Extracted so the arg contract can
/// be asserted without spawning the codex CLI.
fn append_nonstreaming_output_args(cmd: &mut Command, output_path: &str) {
    cmd.arg("--json").arg("-o").arg(output_path).arg("-");
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
                },
                Message {
                    role: Role::User,
                    content: Some(MessageContent::Text("Hello".to_string())),
                    name: None,
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            functions: None,
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: None,
            metadata: Default::default(),
            agent_id: None,
            reasoning_effort: None,
        };

        let prompt = provider.build_prompt(&request);
        assert!(prompt.contains("[System]\nYou are helpful."));
        assert!(prompt.contains("[User]\nHello"));
    }

    /// 回帰: 画像添付（マルチパート）でも本文が消えないこと。以前は
    /// text_content() が Multi に対し None を返すため、画像を貼ると発言が
    /// 丸ごと落ちて「何も届かない」状態になっていた。
    #[test]
    fn test_build_prompt_multipart_preserves_text_and_notes_image() {
        let request = ChatRequest {
            model: "gpt-5.6-sol".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: Some(MessageContent::Multi(vec![
                    ContentPart::Text {
                        text: "この画像を見て".to_string(),
                    },
                    ContentPart::ImageUrl {
                        image_url: ImageUrl {
                            url: "https://cdn.discordapp.com/x.png".to_string(),
                            detail: None,
                        },
                    },
                ])),
                name: None,
                function_call: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            functions: None,
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: None,
            metadata: Default::default(),
            agent_id: None,
            reasoning_effort: None,
        };

        let prompt = build_cli_prompt(&request);
        // 本文は残る（以前は空になっていた）。
        assert!(prompt.contains("この画像を見て"), "text dropped: {prompt}");
        // 画像がある事実は注記される（モデルが状況を把握できる）。
        assert!(prompt.contains("画像"), "image note missing: {prompt}");
        assert!(prompt.contains("[User]"), "user turn missing: {prompt}");
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
            }]),
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: None,
            metadata: Default::default(),
            agent_id: None,
            reasoning_effort: None,
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
                },
                Message {
                    role: Role::Tool,
                    content: Some(MessageContent::Text("done".to_string())),
                    name: Some("send_message".to_string()),
                    function_call: None,
                    tool_calls: None,
                    tool_call_id: Some("call_1".to_string()),
                },
            ],
            functions: None,
            function_call: None,
            temperature: None,
            max_tokens: None,
            stop: None,
            stream: None,
            metadata: Default::default(),
            agent_id: None,
            reasoning_effort: None,
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
    fn test_reasoning_effort_builder() {
        assert!(CodexProvider::new().reasoning_effort.is_none());
        assert!(CodexProvider::new()
            .with_reasoning_effort("")
            .reasoning_effort
            .is_none());
        assert_eq!(
            CodexProvider::new()
                .with_reasoning_effort("medium")
                .reasoning_effort
                .as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn test_resolve_codex_output_nonzero_exit_keeps_response() {
        // 非ゼロ終了でも本文があれば捨てずに使う（gpt-5.6-sol の exit 1 問題）
        let out = resolve_codex_output(
            "exit status: 1",
            false,
            Some("Hi! 👋".to_string()),
            b"",
            b"some agentic warning",
        )
        .expect("non-empty output must be kept even on non-zero exit");
        assert_eq!(out, "Hi! 👋");
    }

    #[test]
    fn test_resolve_codex_output_nonzero_empty_is_error_with_stderr() {
        // 非ゼロ終了 かつ 本文空 は失敗。stderr を握りつぶさないこと。
        let err = resolve_codex_output(
            "exit status: 1",
            false,
            Some("  ".to_string()),
            b"",
            b"boom: real reason",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("boom: real reason"), "{err}");
        assert!(err.contains("exit status: 1"), "{err}");
    }

    #[test]
    fn test_resolve_codex_output_success_uses_file() {
        let out =
            resolve_codex_output("exit status: 0", true, Some("answer".to_string()), b"", b"")
                .unwrap();
        assert_eq!(out, "answer");
    }

    #[test]
    fn test_resolve_codex_output_no_file_falls_back_to_stdout() {
        // -o ファイルが無い場合は stdout を使う（正常終了）
        let out =
            resolve_codex_output("exit status: 0", true, None, b"stdout answer", b"").unwrap();
        assert_eq!(out, "stdout answer");
        // 非ゼロ終了 かつ ファイル無し は失敗（stderr+stdout 付き）
        let err = resolve_codex_output("exit status: 1", false, None, b"partial", b"why it died")
            .unwrap_err()
            .to_string();
        assert!(err.contains("why it died"), "{err}");
    }

    #[test]
    fn test_extra_models() {
        let provider = CodexProvider::new().with_extra_models(vec![("gpt-5".to_string(), 128_000)]);

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let models = rt.block_on(provider.available_models()).unwrap();
        assert!(models.iter().any(|m| m.id == "gpt-5"));
        assert!(models.iter().any(|m| m.id == "o4-mini"));
    }

    #[test]
    fn test_gpt56_in_default_models() {
        let provider = CodexProvider::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let models = rt.block_on(provider.available_models()).unwrap();
        // Codex サブスクでも GPT-5.6 系が選択肢に出ること
        for id in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert!(models.iter().any(|m| m.id == id), "{id} が候補に無い");
        }
    }

    #[test]
    fn test_parse_jsonl_event_agent_message() {
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Hello world"}}"#;
        let delta = parse_jsonl_event(line, "o4-mini").unwrap();
        assert_eq!(
            delta.choices[0].delta.content.as_deref(),
            Some("Hello world")
        );
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

    /// #148 regression: without `--json`, codex emits no `turn.completed` events,
    /// so usage parsing yields all zeros. This documents the exact pre-fix symptom
    /// and pairs with the positive test above / the arg-contract test below.
    #[test]
    fn test_parse_usage_from_stdout_without_json_is_zero() {
        let stdout = b"just the final assistant text, no JSONL events here\n";
        let usage = parse_usage_from_stdout(stdout);
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(usage.total_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 0);
    }

    /// #148 regression: the non-streaming chat_completion invocation must pass
    /// `--json` (before `-o`) so usage can be accounted. Guards against the flag
    /// being dropped, which would silently zero out codex token accounting.
    #[test]
    fn test_nonstreaming_command_includes_json_flag() {
        let mut cmd = Command::new("codex");
        append_nonstreaming_output_args(&mut cmd, "/tmp/out.txt");
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter().any(|a| a == "--json"),
            "non-streaming codex command must include --json for usage accounting (#148): {args:?}"
        );
        let json_pos = args.iter().position(|a| a == "--json").unwrap();
        let o_pos = args.iter().position(|a| a == "-o").unwrap();
        assert!(json_pos < o_pos, "--json must precede -o: {args:?}");
        // -o still writes the final message file; prompt still comes from stdin (`-`).
        assert_eq!(
            args.get(o_pos + 1).map(String::as_str),
            Some("/tmp/out.txt")
        );
        assert!(
            args.iter().any(|a| a == "-"),
            "prompt must be read from stdin"
        );
    }
}
