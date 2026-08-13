use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tracing;

use super::config::{CommandPermission, ShellToolConfig};
use crate::traits::{Action, ActionContext, ActionResult, CallerIdentity};

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
        "Execute a shell command from the allowed list. Returns full stdout, stderr, exit_code, and a truncated flag (always false; output is never truncated at the source)."
    }

    fn parameters(&self) -> serde_json::Value {
        let allowed: Vec<String> = self
            .config
            .effective_commands()
            .iter()
            .map(|c| c.name.clone())
            .collect();
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
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": format!(
                        "Optional timeout in seconds for this call (default: {}, max: {}). \
                         Use a larger value for long-running commands instead of \
                         backgrounding them.",
                        self.config.timeout_secs, self.config.max_timeout_secs
                    )
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: &serde_json::Value, ctx: &ActionContext) -> ActionResult {
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

        // 宣言された permission は「実行に必要な caller クラス」を表す。誰が誰より上位かの
        // 序列は shell.rs では判断せず、唯一の源である caller.rs の trust_level に委ねる
        // （#485 で co_agent を owner 等価へ引き上げた序列: owner = co_agent > trusted_user
        // > agent）。ここでは permission を対応する CallerIdentity へ写すだけで、判定は
        // trust_level の比較 1 本（caller.rs の `can_manage_subtask_of` と同じ形）。
        // これにより shell.rs 独自の旧序列（owner > agent > co_agent）が源と食い違って
        // co_agent だけ agent 級を実行できない、という #608 の逆転を構造的に防ぐ。
        let required_caller = match cmd_config.permission {
            CommandPermission::Owner => CallerIdentity::Owner,
            CommandPermission::Agent => CallerIdentity::Agent,
            // agent_id は trust_level に影響しないためダミーで良い。
            CommandPermission::CoAgent => CallerIdentity::CoAgent {
                agent_id: String::new(),
            },
        };
        let permitted = ctx.caller.trust_level() >= required_caller.trust_level();

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
        // タイムアウトで future が drop されたとき子プロセスを確実に kill する。
        // これが無いとハングしたコマンドがタイムアウト後も走り続け、
        // 孤児プロセスの蓄積やロック保持を招く。
        cmd.kill_on_drop(true);
        cmd.args(&cmd_args);
        cmd.current_dir(ctx.workspace.root());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        if stdin_input.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }

        // Environment setup
        if self.config.inherit_env {
            // inherit_env=true: 親の全環境を明示的に継承（SSH_AUTH_SOCK 等）。
            // tokio の暗黙のデフォルト継承に頼らず明示的に渡すことで、
            // 将来 env_clear が他経路で呼ばれても確実に継承される。
            cmd.envs(std::env::vars());
        } else {
            // inherit_env=false: allowlist のみ。それ以外の親環境は子に渡さない。
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

        // Wait with timeout。優先順位: 呼び出し時の timeout_secs 引数（最も具体的）>
        // コマンド個別設定 > グローバル既定。呼び出し時指定は LLM 由来なので
        // [1, max_timeout_secs] にクランプする（無制限の占有を防ぐ）。
        // - max_timeout_secs=0 という誤設定でも panic しない（clamp は min>max で assert）
        // - LLM は数値を文字列で送ることがあるため数字文字列も受け付ける
        // - 存在するのに解釈できない値は黙って既定にフォールバックせずエラーで返す
        //   （既定で走ってタイムアウト死するより、即時修正できる失敗のほうが良い）
        let requested_timeout = match args.get("timeout_secs") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => {
                let parsed = v
                    .as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()));
                match parsed {
                    Some(t) => Some(t.clamp(1, self.config.max_timeout_secs.max(1))),
                    None => {
                        return ActionResult::error(&format!(
                            "Invalid timeout_secs: {v} (expected a positive integer number of seconds)"
                        ));
                    }
                }
            }
        };
        let timeout_secs = requested_timeout
            .or(cmd_config.timeout_secs)
            .unwrap_or(self.config.timeout_secs);
        let timeout_duration = std::time::Duration::from_secs(timeout_secs);

        // stdin の書き込みは出力読み取りと**並行**に行い、全体をタイムアウトで包む。
        // 以前は「stdin を全部書いてから wait_with_output」だったため、
        // (1) 子が stdout を書き始めてパイプバッファ（64KB）が埋まると、stdin 待ちの
        //     子と stdin 書き込み中の親が相互待ちでデッドロックし、
        // (2) タイムアウトは wait 側にしか掛かっていなかったので永遠にハングした
        //     （stdin がパイプバッファ超の場合に確実に発生）。
        let stdin_handle = child.stdin.take();
        let io_fut = async {
            let write_stdin = async {
                if let (Some(input), Some(mut handle)) = (stdin_input, stdin_handle) {
                    if let Err(e) = handle.write_all(input.as_bytes()).await {
                        tracing::warn!("Failed to write stdin: {}", e);
                    }
                    // drop で閉じられ子に EOF が伝わる
                }
            };
            let (out, ()) = tokio::join!(child.wait_with_output(), write_stdin);
            out
        };
        let output = match tokio::time::timeout(timeout_duration, io_fut).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return ActionResult::error(&format!("Command execution failed: {}", e)),
            Err(_) => {
                // kill_on_drop により子プロセスはここで kill される。
                return ActionResult::error(&format!(
                    "Command timed out after {} seconds",
                    timeout_secs
                ));
            }
        };

        // No source-level truncation: stdout/stderr are passed through in full so that
        // downstream consumers (the LLM and the webhook layer) receive every byte. The
        // webhook layer performs lossless, ordered chunking (`build_tool_event_message`)
        // to satisfy Discord size limits, so we must not drop the tail here.
        // Only lossy step is `from_utf8_lossy`, which replaces invalid UTF-8 sequences
        // rather than dropping bytes — no bytes are silently discarded.
        let stdout_str = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr_str = String::from_utf8_lossy(&output.stderr).into_owned();
        // `truncated` is retained for wire/consumer compatibility but is always false:
        // full output is preserved, so it must never falsely advertise data loss.
        let truncated = false;
        let exit_code = output.status.code().unwrap_or(-1);

        ActionResult::success(json!({
            "stdout": stdout_str,
            "stderr": stderr_str,
            "exit_code": exit_code,
            "truncated": truncated
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::config::{CommandConfig, CommandPermission, ShellToolConfig};
    use crate::traits::{ActionContext, CallerIdentity, RuntimeInfo};
    use std::sync::{Arc, Mutex};

    fn make_ctx(caller: CallerIdentity) -> (tempfile::TempDir, ActionContext) {
        let conn = opencrab_db::init_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
        let ctx = ActionContext {
            agent_id: "test-agent".to_string(),
            agent_name: "Test Agent".to_string(),
            session_id: None,
            db: opencrab_db::Db::from_connection(conn),
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
            commands: vec![CommandConfig {
                name: "rm".to_string(),
                permission: CommandPermission::Owner,
                timeout_secs: None,
                description: Some("Dangerous".to_string()),
            }],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Agent);
        let args =
            serde_json::json!({"command": "rm", "args": ["-f", "/tmp/nonexistent_test_file_xyz"]});
        let result = action.execute(&args, &ctx).await;
        assert!(
            !result.success,
            "Agent should not be able to run owner-only command"
        );
        assert!(
            result.error.as_deref().unwrap_or("").contains("ermission"),
            "Error should mention permission: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn test_owner_can_run_owner_command() {
        let config = ShellToolConfig {
            commands: vec![CommandConfig {
                name: "echo".to_string(),
                permission: CommandPermission::Owner,
                timeout_secs: None,
                description: None,
            }],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args = serde_json::json!({"command": "echo", "args": ["hello"]});
        let result = action.execute(&args, &ctx).await;
        assert!(
            result.success,
            "Owner should be able to run owner-level command"
        );
    }

    #[tokio::test]
    async fn test_inherit_env_passes_custom_parent_var() {
        // When inherit_env is enabled, a custom var set in the parent process
        // must be visible to the child.
        std::env::set_var("OPENCRAB_TEST_CUSTOM_VAR", "hello123");
        let config = ShellToolConfig {
            inherit_env: true,
            commands: vec![CommandConfig {
                name: "printenv".to_string(),
                permission: CommandPermission::Agent,
                timeout_secs: None,
                description: None,
            }],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args = serde_json::json!({"command": "printenv", "args": ["OPENCRAB_TEST_CUSTOM_VAR"]});
        let result = action.execute(&args, &ctx).await;
        assert!(
            result.success,
            "printenv should succeed: {:?}",
            result.error
        );
        let stdout = result
            .data
            .as_ref()
            .and_then(|d| d.get("stdout"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            stdout.contains("hello123"),
            "child should see inherited custom var, got: {:?}",
            stdout
        );
    }

    #[tokio::test]
    async fn test_inherit_env_passes_ssh_auth_sock() {
        // SSH_AUTH_SOCK-like var must be inherited when inherit_env is enabled.
        std::env::set_var("SSH_AUTH_SOCK", "/tmp/opencrab-test-agent.sock");
        let config = ShellToolConfig {
            inherit_env: true,
            commands: vec![CommandConfig {
                name: "printenv".to_string(),
                permission: CommandPermission::Agent,
                timeout_secs: None,
                description: None,
            }],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args = serde_json::json!({"command": "printenv", "args": ["SSH_AUTH_SOCK"]});
        let result = action.execute(&args, &ctx).await;
        assert!(
            result.success,
            "printenv should succeed: {:?}",
            result.error
        );
        let stdout = result
            .data
            .as_ref()
            .and_then(|d| d.get("stdout"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            stdout.contains("/tmp/opencrab-test-agent.sock"),
            "child should inherit SSH_AUTH_SOCK, got: {:?}",
            stdout
        );
    }

    #[tokio::test]
    async fn test_restrictive_allowlist_passes_only_allowed_vars() {
        // When inherit_env is disabled, only allow-listed vars reach the child.
        std::env::set_var("OPENCRAB_TEST_ALLOWED", "yes-allowed");
        std::env::set_var("OPENCRAB_TEST_BLOCKED", "no-blocked");
        let config = ShellToolConfig {
            inherit_env: false,
            // PATH is required so the child can resolve the `env` binary.
            allowed_env_vars: vec!["PATH".to_string(), "OPENCRAB_TEST_ALLOWED".to_string()],
            commands: vec![CommandConfig {
                name: "env".to_string(),
                permission: CommandPermission::Agent,
                timeout_secs: None,
                description: None,
            }],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args = serde_json::json!({"command": "env"});
        let result = action.execute(&args, &ctx).await;
        assert!(result.success, "env should succeed: {:?}", result.error);
        let stdout = result
            .data
            .as_ref()
            .and_then(|d| d.get("stdout"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            stdout.contains("OPENCRAB_TEST_ALLOWED=yes-allowed"),
            "allow-listed var should be passed through, got: {:?}",
            stdout
        );
        assert!(
            !stdout.contains("OPENCRAB_TEST_BLOCKED"),
            "non-allow-listed var must NOT leak to child, got: {:?}",
            stdout
        );
    }

    #[tokio::test]
    async fn test_restrictive_allowlist_passes_ssh_auth_sock() {
        // When inherit_env is disabled but SSH_AUTH_SOCK is allow-listed,
        // the parent's SSH_AUTH_SOCK must reach the child. Use a unique var
        // name to avoid colliding with the inherit-mode SSH test under
        // parallel (process-global env) execution.
        std::env::set_var(
            "OPENCRAB_TEST_SSH_AUTH_SOCK",
            "/tmp/opencrab-allowlist-agent.sock",
        );
        let config = ShellToolConfig {
            inherit_env: false,
            allowed_env_vars: vec![
                "PATH".to_string(),
                "OPENCRAB_TEST_SSH_AUTH_SOCK".to_string(),
            ],
            commands: vec![CommandConfig {
                name: "printenv".to_string(),
                permission: CommandPermission::Agent,
                timeout_secs: None,
                description: None,
            }],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args =
            serde_json::json!({"command": "printenv", "args": ["OPENCRAB_TEST_SSH_AUTH_SOCK"]});
        let result = action.execute(&args, &ctx).await;
        assert!(
            result.success,
            "printenv should succeed: {:?}",
            result.error
        );
        let stdout = result
            .data
            .as_ref()
            .and_then(|d| d.get("stdout"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            stdout.contains("/tmp/opencrab-allowlist-agent.sock"),
            "allow-listed SSH_AUTH_SOCK-like var should reach child, got: {:?}",
            stdout
        );
    }

    /// Build a config whose `max_output_bytes` is deliberately *smaller* than the
    /// output we will generate, proving the value no longer governs truncation.
    fn small_limit_config(cmd: &str) -> ShellToolConfig {
        ShellToolConfig {
            // Smaller than the old 64 KiB default and far smaller than test output,
            // to prove this knob no longer truncates anything.
            max_output_bytes: 1024,
            commands: vec![CommandConfig {
                name: cmd.to_string(),
                permission: CommandPermission::Agent,
                timeout_secs: None,
                description: None,
            }],
            ..ShellToolConfig::default()
        }
    }

    #[tokio::test]
    async fn test_stdout_longer_than_old_limit_is_preserved_with_tail() {
        // Generate well over the old 64 KiB cap on stdout via a unique, checkable tail.
        const OLD_LIMIT: usize = 65536;
        let total = OLD_LIMIT + 4096;
        // `printf` with a wide field of '#' plus a distinctive tail marker.
        let head = "#".repeat(total - "TAIL_MARKER_END".len());
        let payload = format!("{head}TAIL_MARKER_END");
        let config = small_limit_config("printf");
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args = serde_json::json!({"command": "printf", "args": ["%s", payload]});
        let result = action.execute(&args, &ctx).await;
        assert!(result.success, "printf should succeed: {:?}", result.error);
        let data = result.data.as_ref().unwrap();
        let stdout = data.get("stdout").and_then(|v| v.as_str()).unwrap();
        assert_eq!(
            stdout.len(),
            payload.len(),
            "full stdout must be preserved, no byte loss"
        );
        assert!(
            stdout.len() > OLD_LIMIT,
            "output must exceed the old 64 KiB limit to be meaningful"
        );
        assert!(
            stdout.ends_with("TAIL_MARKER_END"),
            "tail of stdout must survive (no head-only truncation)"
        );
        assert_eq!(
            data.get("truncated").and_then(|v| v.as_bool()),
            Some(false),
            "truncated must be false when full output is preserved"
        );
    }

    #[tokio::test]
    async fn test_stderr_longer_than_old_limit_is_preserved_with_tail() {
        const OLD_LIMIT: usize = 65536;
        let total = OLD_LIMIT + 4096;
        let head = "E".repeat(total - "STDERR_TAIL_END".len());
        let payload = format!("{head}STDERR_TAIL_END");
        let config = small_limit_config("sh");
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        // Emit the payload to stderr only.
        let script = format!("printf '%s' '{payload}' 1>&2");
        let args = serde_json::json!({"command": "sh", "args": ["-c", script]});
        let result = action.execute(&args, &ctx).await;
        assert!(result.success, "sh should succeed: {:?}", result.error);
        let data = result.data.as_ref().unwrap();
        let stderr = data.get("stderr").and_then(|v| v.as_str()).unwrap();
        assert_eq!(
            stderr.len(),
            payload.len(),
            "full stderr must be preserved, no byte loss"
        );
        assert!(
            stderr.len() > OLD_LIMIT,
            "stderr must exceed the old 64 KiB limit to be meaningful"
        );
        assert!(
            stderr.ends_with("STDERR_TAIL_END"),
            "tail of stderr must survive (no head-only truncation)"
        );
        assert_eq!(
            data.get("truncated").and_then(|v| v.as_bool()),
            Some(false),
            "truncated must be false when full output is preserved"
        );
    }

    #[tokio::test]
    async fn test_no_truncated_marker_for_ordinary_long_output() {
        // Ordinary long output must never be flagged as truncated nor carry any
        // "[truncated]" sentinel in the payload.
        // 200KB を argv で渡すと ARG_MAX の小さい環境（サンドボックス CI 等）で
        // E2BIG になるため、stdin 経由で cat に流す（テストの意図は「長い出力が
        // 切られない」ことなので同等）。
        let payload = "L".repeat(200_000);
        let config = small_limit_config("cat");
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args = serde_json::json!({"command": "cat", "stdin": payload});
        let result = action.execute(&args, &ctx).await;
        assert!(result.success, "cat should succeed: {:?}", result.error);
        let data = result.data.as_ref().unwrap();
        let stdout = data.get("stdout").and_then(|v| v.as_str()).unwrap();
        assert_eq!(stdout.len(), payload.len(), "no byte loss for long output");
        assert!(
            !stdout.contains("[truncated]"),
            "no [truncated] sentinel must be injected into the output"
        );
        assert_eq!(
            data.get("truncated").and_then(|v| v.as_bool()),
            Some(false),
            "truncated flag must remain false for ordinary long output"
        );
    }

    /// sleep を許可した config（タイムアウト系テスト用）。
    fn sleep_config() -> ShellToolConfig {
        ShellToolConfig {
            commands: vec![CommandConfig {
                name: "sleep".to_string(),
                permission: CommandPermission::Agent,
                timeout_secs: None,
                description: None,
            }],
            ..ShellToolConfig::default()
        }
    }

    #[tokio::test]
    async fn test_per_call_timeout_secs_is_honored() {
        let action = ShellToolAction::new(sleep_config());
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        // グローバル既定（120s）では待たされるところを、呼び出し時 1s 指定で切る
        let args = serde_json::json!({"command": "sleep", "args": ["5"], "timeout_secs": 1});
        let start = std::time::Instant::now();
        let result = action.execute(&args, &ctx).await;
        assert!(!result.success, "must time out");
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timed out after 1 seconds"),
            "error should mention the effective 1s timeout: {:?}",
            result.error
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(4),
            "should return promptly after the 1s timeout, not the global default"
        );
    }

    #[tokio::test]
    async fn test_per_call_timeout_clamped_to_max() {
        let config = ShellToolConfig {
            max_timeout_secs: 1,
            ..sleep_config()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        // 上限 1s の構成で 9999s を要求 → 1s にクランプされて切れる
        let args = serde_json::json!({"command": "sleep", "args": ["5"], "timeout_secs": 9999});
        let result = action.execute(&args, &ctx).await;
        assert!(!result.success, "must time out at the clamped maximum");
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timed out after 1 seconds"),
            "requested timeout must be clamped to max_timeout_secs: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn test_per_call_timeout_overrides_per_command_timeout() {
        // コマンド個別 60s 設定より呼び出し時 1s が優先される
        let config = ShellToolConfig {
            commands: vec![CommandConfig {
                name: "sleep".to_string(),
                permission: CommandPermission::Agent,
                timeout_secs: Some(60),
                description: None,
            }],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args = serde_json::json!({"command": "sleep", "args": ["5"], "timeout_secs": 1});
        let result = action.execute(&args, &ctx).await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timed out after 1 seconds"),
            "per-call timeout must take precedence over per-command config: {:?}",
            result.error
        );
    }

    #[test]
    fn test_default_timeouts_raised() {
        let config = ShellToolConfig::default();
        assert_eq!(config.timeout_secs, 120, "default raised from 30s");
        assert_eq!(
            config.max_timeout_secs, 1800,
            "cap aligned with spawn_subtask default"
        );
        // 既存設定（新フィールド無し）がデシリアライズできて既定が入ること
        let parsed: ShellToolConfig = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert_eq!(parsed.timeout_secs, 120);
        assert_eq!(parsed.max_timeout_secs, 1800);
    }

    #[tokio::test]
    async fn test_max_timeout_zero_does_not_panic() {
        // max_timeout_secs=0 の誤設定 + LLM の timeout_secs 指定で clamp(1,0) が
        // panic していた（レビュー指摘 HIGH）。1s に丸めて動作すること。
        let config = ShellToolConfig {
            max_timeout_secs: 0,
            ..sleep_config()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args = serde_json::json!({"command": "sleep", "args": ["3"], "timeout_secs": 5});
        let result = action.execute(&args, &ctx).await;
        assert!(
            !result.success,
            "should time out (clamped to 1s), not panic"
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timed out after 1 seconds"),
            "must clamp to 1s without panicking: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn test_timeout_secs_as_digit_string_is_accepted() {
        // LLM は数値引数を文字列で送ることがある（レビュー指摘 MED）。
        let action = ShellToolAction::new(sleep_config());
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        let args = serde_json::json!({"command": "sleep", "args": ["5"], "timeout_secs": "1"});
        let result = action.execute(&args, &ctx).await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timed out after 1 seconds"),
            "digit string must be coerced, not silently dropped: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn test_timeout_secs_invalid_value_is_rejected() {
        // 解釈できない値は黙ってフォールバックせずエラーで返す。
        let action = ShellToolAction::new(sleep_config());
        let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
        for bad in [
            serde_json::json!("soon"),
            serde_json::json!(-5),
            serde_json::json!(1.5),
        ] {
            let args = serde_json::json!({"command": "sleep", "args": ["0"], "timeout_secs": bad});
            let result = action.execute(&args, &ctx).await;
            assert!(!result.success, "invalid timeout_secs must be an error");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .contains("Invalid timeout_secs"),
                "error must name the problem: {:?}",
                result.error
            );
        }
    }

    #[test]
    fn test_schema_exposes_timeout_secs() {
        let action = ShellToolAction::new(ShellToolConfig::default());
        let schema = action.parameters();
        assert!(
            schema["properties"]["timeout_secs"].is_object(),
            "timeout_secs must be a declared parameter so the LLM can use it"
        );
    }

    /// #608 回帰: co_agent は owner 等価（#485）なので agent 級コマンドを実行できる。
    /// 旧テスト `test_coagent_cannot_run_agent_command` は #485 以前の序列
    /// （owner > agent > co_agent）を前提にしており、唯一の源（caller.rs）へ追従していない
    /// shell.rs の手書き判定のせいで co_agent だけが取り残されていた。
    #[tokio::test]
    async fn test_coagent_can_run_agent_command() {
        let config = ShellToolConfig {
            commands: vec![CommandConfig {
                name: "echo".to_string(),
                permission: CommandPermission::Agent,
                timeout_secs: None,
                description: None,
            }],
            ..ShellToolConfig::default()
        };
        let action = ShellToolAction::new(config);
        let (_dir, ctx) = make_ctx(CallerIdentity::CoAgent {
            agent_id: "helper-bot".to_string(),
        });
        let args = serde_json::json!({"command": "echo", "args": ["hello"]});
        let result = action.execute(&args, &ctx).await;
        assert!(
            result.success,
            "CoAgent は owner 等価なので agent 級コマンドを実行できる: {:?}",
            result.error
        );
    }

    /// #608: caller × permission の網羅マトリクス。判定は caller.rs の trust_level 序列
    /// （owner = co_agent = 2 > trusted_user = 1 > agent = 0）に委ねており、宣言した
    /// permission 以上の caller だけが実行できる。permission→必要 trust は Owner=2 /
    /// Agent=0 / CoAgent=2（co_agent は owner 等価）。序列が逆転（例: 上位 caller が
    /// 下位より実行できるコマンドが減る）したらここが落ちる。
    #[tokio::test]
    async fn test_permission_ladder_matrix() {
        // 各 permission（Owner / Agent / CoAgent の順）について、その caller が実行を
        // 許可されるべきか。
        let cases: &[(CallerIdentity, [bool; 3])] = &[
            (CallerIdentity::Owner, [true, true, true]),
            (
                CallerIdentity::CoAgent {
                    agent_id: "helper".to_string(),
                },
                [true, true, true],
            ),
            (CallerIdentity::TrustedUser, [false, true, false]),
            (CallerIdentity::Agent, [false, true, false]),
        ];
        let perms = [
            CommandPermission::Owner,
            CommandPermission::Agent,
            CommandPermission::CoAgent,
        ];
        for (caller, expected) in cases {
            for (i, perm) in perms.iter().enumerate() {
                let config = ShellToolConfig {
                    commands: vec![CommandConfig {
                        name: "echo".to_string(),
                        permission: perm.clone(),
                        timeout_secs: None,
                        description: None,
                    }],
                    ..ShellToolConfig::default()
                };
                let action = ShellToolAction::new(config);
                let (_dir, ctx) = make_ctx(caller.clone());
                let args = serde_json::json!({"command": "echo", "args": ["hi"]});
                let result = action.execute(&args, &ctx).await;
                assert_eq!(
                    result.success, expected[i],
                    "caller={:?} perm={:?}: expected permitted={}, got success={} (error={:?})",
                    caller, perm, expected[i], result.success, result.error
                );
                if !expected[i] {
                    assert!(
                        result
                            .error
                            .as_deref()
                            .unwrap_or("")
                            .contains("Permission denied"),
                        "denied case must report a permission error: caller={:?} perm={:?} error={:?}",
                        caller,
                        perm,
                        result.error
                    );
                }
            }
        }
    }
}
