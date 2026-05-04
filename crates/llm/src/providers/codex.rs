use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::debug;

use crate::message::*;
use crate::traits::{LlmProvider, ModelInfo};

const DEFAULT_CODEX_PATH: &str = "codex";
const DEFAULT_MODEL: &str = "o4-mini";
const DEFAULT_TIMEOUT_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct CodexProvider {
    codex_path: String,
    default_model: String,
    sandbox: String,
    working_dir: Option<String>,
    timeout: Duration,
}

impl CodexProvider {
    pub fn new() -> Self {
        Self {
            codex_path: DEFAULT_CODEX_PATH.to_string(),
            default_model: DEFAULT_MODEL.to_string(),
            sandbox: "read-only".to_string(),
            working_dir: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
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

    fn build_prompt(&self, request: &ChatRequest) -> String {
        let mut parts = Vec::new();

        for msg in &request.messages {
            let content = msg.text_content().unwrap_or("").to_string();
            if content.is_empty() {
                continue;
            }
            match msg.role {
                Role::System => parts.push(format!("[System]\n{content}")),
                Role::User => parts.push(format!("[User]\n{content}")),
                Role::Assistant => parts.push(format!("[Assistant]\n{content}")),
                Role::Tool => {
                    let name = msg.name.as_deref().unwrap_or("tool");
                    parts.push(format!("[Tool Result: {name}]\n{content}"));
                }
            }
        }

        parts.join("\n\n")
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
        Ok(vec![
            ModelInfo {
                id: "o4-mini".to_string(),
                name: "o4-mini".to_string(),
                context_window: 200_000,
                supports_function_calling: false,
                supports_vision: false,
            },
            ModelInfo {
                id: "o3".to_string(),
                name: "o3".to_string(),
                context_window: 200_000,
                supports_function_calling: false,
                supports_vision: false,
            },
            ModelInfo {
                id: "codex-mini".to_string(),
                name: "codex-mini".to_string(),
                context_window: 200_000,
                supports_function_calling: false,
                supports_vision: false,
            },
        ])
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

        let mut cmd = Command::new(&self.codex_path);
        cmd.arg("exec")
            .arg("--ephemeral")
            .arg("--skip-git-repo-check")
            .arg("-m")
            .arg(model)
            .arg("-s")
            .arg(&self.sandbox)
            .arg("-o")
            .arg(&output_path)
            // Never prompt for approval (non-interactive).
            .arg("-a")
            .arg("never")
            // Read prompt from stdin to avoid ARG_MAX limits and /proc exposure.
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(ref dir) = self.working_dir {
            cmd.arg("-C").arg(dir);
        }

        let mut child = cmd.spawn().context("failed to spawn codex CLI")?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .context("failed to write prompt to codex stdin")?;
            // Drop stdin to signal EOF.
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
            usage: Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            created: chrono::Utc::now().timestamp(),
        })
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
}
