//! `execute_shell` の呼び方の誤り（起動前に弾く診断）。

use super::{make_ctx, stdout_of};
use crate::tools::config::{CommandConfig, CommandPermission, ShellToolConfig};
use crate::traits::{Action, ActionResult, CallerIdentity};

use super::super::ShellToolAction;

/// QC 2026-10-20 の実害（くらぶ）: モデルが `{"command":"","args":["--help"]}` を送り、
/// `Command::new("")` が `os error 2` で落ちた。利用者には「許可が無い」のか
/// 「実行ファイルが無い」のか分からない文面しか返らなかった。起動前に弾き、
/// 正しい呼び方（`command` にプログラム名、`args` に引数）を返す。
fn d1089_action_with(commands: &[&str]) -> ShellToolAction {
    ShellToolAction::new(ShellToolConfig {
        commands: commands
            .iter()
            .map(|name| CommandConfig {
                name: (*name).to_string(),
                permission: CommandPermission::Agent,
                timeout_secs: None,
                description: None,
            })
            .collect(),
        ..ShellToolConfig::default()
    })
}

fn error_of(result: &ActionResult) -> String {
    result.error.clone().unwrap_or_default()
}

#[tokio::test]
async fn test_empty_command_is_rejected_before_spawn() {
    let action = d1089_action_with(&["echo"]);
    let (_ws, ctx) = make_ctx(CallerIdentity::Agent);
    let result = action
        .execute(
            &serde_json::json!({"command": "", "args": ["--help"]}),
            &ctx,
        )
        .await;

    assert!(!result.success, "an empty command must not run");
    let error = error_of(&result);
    assert!(
        error.contains("requires `command`"),
        "error must name the missing field: {error:?}"
    );
    assert!(
        error.contains("`args`"),
        "error must explain where arguments go: {error:?}"
    );
    assert!(
        !error.contains("Failed to spawn") && !error.contains("os error"),
        "an empty command must be rejected before spawn, not by the OS: {error:?}"
    );
}

#[tokio::test]
async fn test_whitespace_only_command_is_rejected_before_spawn() {
    let action = d1089_action_with(&["echo"]);
    let (_ws, ctx) = make_ctx(CallerIdentity::Agent);
    let result = action
        .execute(&serde_json::json!({"command": "   ", "args": []}), &ctx)
        .await;

    assert!(!result.success, "a whitespace-only command must not run");
    let error = error_of(&result);
    assert!(
        error.contains("requires `command`"),
        "error must name the missing field: {error:?}"
    );
    assert!(
        !error.contains("Failed to spawn") && !error.contains("os error"),
        "a whitespace-only command must be rejected before spawn: {error:?}"
    );
}

#[tokio::test]
async fn test_empty_command_names_a_program_left_in_args() {
    let action = d1089_action_with(&["nostaro"]);
    let (_ws, ctx) = make_ctx(CallerIdentity::Agent);
    let result = action
        .execute(&serde_json::json!({"args": ["nostaro", "--help"]}), &ctx)
        .await;

    assert!(!result.success, "the program in args must not be executed");
    let error = error_of(&result);
    assert!(
        error.contains("`nostaro`"),
        "error must name the program found in args: {error:?}"
    );
    assert!(
        error.contains("command=\"nostaro\"") && error.contains("args=[\"--help\"]"),
        "error must show the corrected call: {error:?}"
    );
    assert!(
        !error.contains("os error"),
        "must not report an OS spawn failure: {error:?}"
    );
}

#[tokio::test]
async fn test_empty_command_names_a_path_left_in_args() {
    let action = d1089_action_with(&["echo"]);
    let (_ws, ctx) = make_ctx(CallerIdentity::Agent);
    let result = action
        .execute(
            &serde_json::json!({"command": "", "args": ["/bin/echo"]}),
            &ctx,
        )
        .await;

    assert!(!result.success, "a path in args must not be executed");
    let error = error_of(&result);
    assert!(
        error.contains("/bin/echo"),
        "error must name the path found in args: {error:?}"
    );
    assert!(
        !error.contains("os error"),
        "must not report an OS spawn failure: {error:?}"
    );
}

/// 正常な呼び方が壊れていないこと（この修正は起動経路を変えない）。
#[tokio::test]
async fn test_valid_command_with_args_is_unaffected() {
    let action = d1089_action_with(&["echo"]);
    let (_ws, ctx) = make_ctx(CallerIdentity::Agent);
    let result = action
        .execute(
            &serde_json::json!({"command": "echo", "args": ["d1089-ok"]}),
            &ctx,
        )
        .await;

    assert!(
        result.success,
        "a valid call must still run: {:?}",
        result.error
    );
    assert!(
        stdout_of(&result).contains("d1089-ok"),
        "unexpected stdout: {:?}",
        stdout_of(&result)
    );
}
