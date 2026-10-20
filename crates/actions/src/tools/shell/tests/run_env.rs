//! D-1091: per-binding environment declared by the gateway.
//!
//! A gateway declares the environment for its own binding; core passes it to the child of
//! `execute_shell` verbatim and interprets nothing. The declared value wins over both the
//! inherited parent environment and the config allow-list, because only the gateway knows which
//! account a binding belongs to. An empty declaration keeps today's behaviour exactly.

use super::*;
use crate::tools::config::{CommandConfig, ShellToolConfig};
use crate::traits::CallerIdentity;

fn printenv_config(run_env: &[(&str, &str)]) -> ShellToolConfig {
    ShellToolConfig {
        inherit_env: false,
        commands: vec![CommandConfig {
            name: "printenv".to_string(),
            permission: CommandPermission::Agent,
            timeout_secs: None,
            description: None,
        }],
        run_env: run_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..ShellToolConfig::default()
    }
}

async fn printenv_var(config: ShellToolConfig, name: &str) -> ActionResult {
    let action = ShellToolAction::new(config);
    let (_dir, ctx) = make_ctx(CallerIdentity::Owner);
    let args = serde_json::json!({"command": "printenv", "args": [name]});
    action.execute(&args, &ctx).await
}

fn stdout_of(result: &ActionResult) -> String {
    result
        .data
        .as_ref()
        .and_then(|d| d.get("stdout"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// A key the gateway declared reaches the child process.
#[tokio::test]
async fn a_declared_env_reaches_the_child() {
    let result = printenv_var(
        printenv_config(&[("OPENCRAB_TEST_BINDING_KEY", "declared-value")]),
        "OPENCRAB_TEST_BINDING_KEY",
    )
    .await;
    assert!(
        result.success,
        "printenv should succeed: {:?}",
        result.error
    );
    assert_eq!(
        stdout_of(&result).trim(),
        "declared-value",
        "the declared value must reach the child"
    );
}

/// A key nobody declared does not reach the child, even when the parent has it.
#[tokio::test]
async fn an_undeclared_env_does_not_reach_the_child() {
    std::env::set_var("OPENCRAB_TEST_UNDECLARED_KEY", "parent-only");
    let result = printenv_var(
        printenv_config(&[("OPENCRAB_TEST_BINDING_KEY", "declared-value")]),
        "OPENCRAB_TEST_UNDECLARED_KEY",
    )
    .await;
    assert!(
        result.success,
        "printenv should succeed: {:?}",
        result.error
    );
    assert_eq!(
        stdout_of(&result),
        "",
        "an undeclared parent variable must not leak into the child"
    );
}

/// The declared value wins over an inherited parent value of the same name.
#[tokio::test]
async fn a_declared_env_overrides_the_inherited_value() {
    std::env::set_var("OPENCRAB_TEST_OVERRIDE_KEY", "from-parent");
    let mut config = printenv_config(&[("OPENCRAB_TEST_OVERRIDE_KEY", "from-binding")]);
    config.inherit_env = true;
    let result = printenv_var(config, "OPENCRAB_TEST_OVERRIDE_KEY").await;
    assert!(
        result.success,
        "printenv should succeed: {:?}",
        result.error
    );
    assert_eq!(
        stdout_of(&result).trim(),
        "from-binding",
        "the binding declaration wins over the inherited parent value"
    );
}

/// An empty declaration behaves exactly as before: the config allow-list still applies.
#[tokio::test]
async fn an_empty_declaration_keeps_the_allow_list_behaviour() {
    std::env::set_var("OPENCRAB_TEST_ALLOWED_KEY", "from-allow-list");
    let mut config = printenv_config(&[]);
    config.allowed_env_vars = vec!["PATH".to_string(), "OPENCRAB_TEST_ALLOWED_KEY".to_string()];
    let result = printenv_var(config, "OPENCRAB_TEST_ALLOWED_KEY").await;
    assert!(
        result.success,
        "printenv should succeed: {:?}",
        result.error
    );
    assert_eq!(
        stdout_of(&result).trim(),
        "from-allow-list",
        "with no declaration the allow-list behaviour is unchanged"
    );
}

/// Core refuses to hand its own provider credentials to a child, even when a binding declares
/// them: those are the process's own outbound credentials, not the binding's account.
#[tokio::test]
async fn a_core_owned_credential_is_not_overridable() {
    let result = printenv_var(
        printenv_config(&[("OPENAI_API_KEY", "declared-but-refused")]),
        "OPENAI_API_KEY",
    )
    .await;
    assert!(
        result.success,
        "printenv should succeed: {:?}",
        result.error
    );
    assert_eq!(
        stdout_of(&result),
        "",
        "a core-owned credential must not be injected from a binding declaration"
    );
}

/// The declaration is filtered before it is applied, so a denied key is reported as refused
/// rather than silently accepted.
#[test]
fn declared_keys_report_which_are_applied_and_which_are_refused() {
    let config = printenv_config(&[
        ("OPENCRAB_TEST_BINDING_KEY", "ok"),
        ("ANTHROPIC_API_KEY", "refused"),
    ]);
    let (applied, refused) = config.declared_env_split();
    assert_eq!(applied, vec!["OPENCRAB_TEST_BINDING_KEY".to_string()]);
    assert_eq!(refused, vec!["ANTHROPIC_API_KEY".to_string()]);
}

/// The log payload of an applied declaration carries names only, never values.
///
/// `declared_env_log_names` is the single source of that payload (see the `debug!` in
/// `shell.rs`), so asserting on it is a direct test of what a log line could ever contain — and
/// unlike capturing the subscriber it cannot be silently disabled by another test's global
/// filter.
#[test]
fn the_logged_payload_carries_names_only() {
    let secret = "binding-account-secret-value";
    let config = printenv_config(&[
        ("OPENCRAB_TEST_BINDING_KEY", secret),
        ("ANTHROPIC_API_KEY", "refused-secret"),
    ]);
    let logged = config.declared_env_log_names();
    assert_eq!(logged, vec!["OPENCRAB_TEST_BINDING_KEY".to_string()]);
    assert!(
        !logged.iter().any(|entry| entry.contains(secret)),
        "a declared value must never reach the log payload: {logged:?}"
    );
    assert!(
        !logged.iter().any(|entry| entry.contains("refused-secret")),
        "a refused value must never reach the log payload: {logged:?}"
    );
}
