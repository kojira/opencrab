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
mod tests;
