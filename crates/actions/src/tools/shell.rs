use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tracing;

use crate::traits::{Action, ActionContext, ActionResult, CallerIdentity};
use super::config::{ShellToolConfig, CommandPermission};

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
        let allowed: Vec<String> = self.config.effective_commands().iter().map(|c| c.name.clone()).collect();
        let allowed_str = allowed.join(", ");
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": format!("Command to execute. Allowed: {}", allowed_str)
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
        ctx: &ActionContext,
    ) -> ActionResult {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return ActionResult::error("Missing required field: command"),
        };

        // Permission-based check
        let effective = self.config.effective_commands();
        let cmd_config = match effective.iter().find(|c| c.name == command) {
            Some(c) => c,
            None => {
                let allowed: Vec<&str> = effective.iter().map(|c| c.name.as_str()).collect();
                return ActionResult::error(&format!(
                    "Command '{}' is not in the allowed list. Allowed: {:?}",
                    command, allowed
                ));
            }
        };

        // Check caller permission against command permission
        let permitted = match &ctx.caller {
            CallerIdentity::Owner => true, // Owner can run everything
            CallerIdentity::Agent => {
                // Agent can run Agent and CoAgent level commands
                cmd_config.permission == CommandPermission::Agent
                    || cmd_config.permission == CommandPermission::CoAgent
            }
            CallerIdentity::CoAgent { .. } => {
                // CoAgent can only run CoAgent level commands
                cmd_config.permission == CommandPermission::CoAgent
            }
        };

        if !permitted {
            return ActionResult::error(&format!(
                "Permission denied: '{}' requires {:?} permission, caller is {:?}",
                command, cmd_config.permission, ctx.caller
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{ActionContext, CallerIdentity, RuntimeInfo};
    use crate::tools::config::{CommandConfig, CommandPermission, ShellToolConfig};
    use std::sync::{Arc, Mutex};

    fn make_ctx(caller: CallerIdentity) -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "test-agent".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: None,
            db: Arc::new(Mutex::new(conn)),
            workspace: Arc::new(ws),
            last_metrics_id: Arc::new(Mutex::new(None)),
            model_override: Arc::new(Mutex::new(None)),
            current_purpose: Arc::new(Mutex::new("test".to_string())),
            runtime_info: Arc::new(Mutex::new(RuntimeInfo {
                default_model: "test".to_string(),
                active_model: None,
                available_providers: vec![],
                gateway: "test".to_string(),
            })),
            caller,
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn test_agent_cannot_run_owner_command() {
        let config = ShellToolConfig {
            commands: vec![
                CommandConfig {
                    name: "rm".to_string(),
                    permission: CommandPermission::Owner,
                    timeout_secs: None,
                    description: Some("Dangerous".to_string()),
                },
            ],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Agent);
        let args = serde_json::json!({"command": "rm", "args": ["-f", "/tmp/nonexistent_test_file_xyz"]});
        let result = action.execute(&args, &ctx).await;
        assert!(!result.success, "Agent should not be able to run owner-only command");
        assert!(
            result.error.as_deref().unwrap_or("").contains("ermission"),
            "Error should mention permission: {:?}", result.error
        );
    }

    #[tokio::test]
    async fn test_owner_can_run_owner_command() {
        let config = ShellToolConfig {
            commands: vec![
                CommandConfig {
                    name: "echo".to_string(),
                    permission: CommandPermission::Owner,
                    timeout_secs: None,
                    description: None,
                },
            ],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args = serde_json::json!({"command": "echo", "args": ["hello"]});
        let result = action.execute(&args, &ctx).await;
        assert!(result.success, "Owner should be able to run owner-level command");
    }

    #[tokio::test]
    async fn test_coagent_cannot_run_agent_command() {
        let config = ShellToolConfig {
            commands: vec![
                CommandConfig {
                    name: "echo".to_string(),
                    permission: CommandPermission::Agent,
                    timeout_secs: None,
                    description: None,
                },
            ],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::CoAgent { agent_id: "helper-bot".to_string() });
        let args = serde_json::json!({"command": "echo", "args": ["hello"]});
        let result = action.execute(&args, &ctx).await;
        assert!(!result.success, "CoAgent should not be able to run agent-level command");
    }
}
