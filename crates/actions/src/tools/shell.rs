use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tracing;

use crate::traits::{Action, ActionContext, ActionResult};
use super::config::ShellToolConfig;

pub struct ShellToolAction {
    pub config: ShellToolConfig,
}

impl ShellToolAction {
    pub fn new(config: ShellToolConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Action for ShellToolAction {
    fn name(&self) -> &str {
        "execute_shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command from the allowed list. Returns stdout, stderr, exit_code, and truncated flag."
    }

    fn parameters(&self) -> serde_json::Value {
        let allowed = self.config.allowed_commands.join(", ");
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": format!("Command to execute. Allowed: {}", allowed)
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Command arguments"
                },
                "stdin": {
                    "type": "string",
                    "description": "Optional stdin input"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        args: &serde_json::Value,
        _ctx: &ActionContext,
    ) -> ActionResult {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return ActionResult::error("Missing required field: command"),
        };

        // Whitelist check
        if !self.config.allowed_commands.contains(&command) {
            return ActionResult::error(&format!(
                "Command {} is not in the allowed list. Allowed: {:?}",
                command, self.config.allowed_commands
            ));
        }

        let cmd_args: Vec<String> = args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        let stdin_input = args
            .get("stdin")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut cmd = tokio::process::Command::new(&command);
        cmd.args(&cmd_args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        if stdin_input.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }

        // Environment setup
        if !self.config.inherit_env {
            cmd.env_clear();
            for var in &self.config.allowed_env_vars {
                if let Ok(val) = std::env::var(var) {
                    cmd.env(var, val);
                }
            }
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ActionResult::error(&format!("Failed to spawn command: {}", e)),
        };

        // Write stdin if provided
        if let Some(input) = stdin_input {
            if let Some(mut stdin_handle) = child.stdin.take() {
                if let Err(e) = stdin_handle.write_all(input.as_bytes()).await {
                    tracing::warn!("Failed to write stdin: {}", e);
                }
            }
        }

        // Wait with timeout
        let timeout_duration = std::time::Duration::from_secs(self.config.timeout_secs);
        let output = match tokio::time::timeout(timeout_duration, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                return ActionResult::error(&format!("Command execution failed: {}", e))
            }
            Err(_) => {
                return ActionResult::error(&format!(
                    "Command timed out after {} seconds",
                    self.config.timeout_secs
                ))
            }
        };

        let max_bytes = self.config.max_output_bytes;

        let (stdout_str, stdout_truncated) = truncate_bytes(&output.stdout, max_bytes);
        let (stderr_str, stderr_truncated) = truncate_bytes(&output.stderr, max_bytes);
        let truncated = stdout_truncated || stderr_truncated;
        let exit_code = output.status.code().unwrap_or(-1);

        ActionResult::success(json!({
            "stdout": stdout_str,
            "stderr": stderr_str,
            "exit_code": exit_code,
            "truncated": truncated
        }))
    }
}

fn truncate_bytes(bytes: &[u8], max: usize) -> (String, bool) {
    if bytes.len() <= max {
        (String::from_utf8_lossy(bytes).into_owned(), false)
    } else {
        (String::from_utf8_lossy(&bytes[..max]).into_owned(), true)
    }
}
