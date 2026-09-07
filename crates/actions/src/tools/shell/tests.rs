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
    let args = serde_json::json!({"command": "printenv", "args": ["OPENCRAB_TEST_SSH_AUTH_SOCK"]});
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
