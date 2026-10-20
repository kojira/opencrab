//! D-1091: the gateway's per-binding environment reaches the child of `execute_shell`.
//!
//! The seam is `build_turn_executor`, the same function production uses to assemble a turn. A
//! gateway declares the environment in `hello` (`opencrab_extgate::protocol::Hello::run_env`) and
//! the turn hands it to the shell tool. These tests run the real child through that seam, so a
//! helper cannot stand in for the production path.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::wiring::{build_turn_executor, TurnExecutorWiring};
use opencrab_core::engine::types::ActionExecutor;

const AGENT: &str = "agent-binding-env";
const DECLARED_NAME: &str = "OPENCRAB_TEST_BINDING_ACCOUNT";

fn owner_context(
    state: &crate::AppState,
    workspace_root: &std::path::Path,
) -> opencrab_actions::ActionContext {
    let workspace = opencrab_core::workspace::Workspace::from_root(workspace_root).unwrap();
    opencrab_actions::ActionContext {
        caller: opencrab_actions::CallerIdentity::Owner,
        agent_id: AGENT.to_string(),
        agent_name: "Binding Env".to_string(),
        session_id: Some("session-binding-env".to_string()),
        db: state.db.clone(),
        workspace: Arc::new(workspace),
        last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
        model_override: Arc::new(std::sync::Mutex::new(None)),
        current_purpose: Arc::new(std::sync::Mutex::new("test".to_string())),
        runtime_info: Arc::new(std::sync::Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "mock:test".to_string(),
            active_model: None,
            available_providers: vec![],
            gateway: "test".to_string(),
        })),
    }
}

/// Build a turn executor for `run_env` and run `printenv <name>` through it.
async fn printenv_with_run_env(run_env: BTreeMap<String, String>) -> (bool, String) {
    let state = crate::test_app_state();
    {
        let mut tools = state.tools_config.write().unwrap();
        tools.enabled = true;
        tools.shell = Some(opencrab_actions::tools::ShellToolConfig {
            enabled: true,
            ..Default::default()
        });
    }
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, AGENT, "printenv", "test").unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let context = owner_context(&state, dir.path());
    let executor = build_turn_executor(
        &state,
        TurnExecutorWiring {
            context,
            depth: 0,
            gateway_actions: None,
            subtask_registry: Arc::new(dashmap::DashMap::new()),
            completion_sink: None,
            reply_target: None,
            tool_allowlist: None,
            tool_event_sink: None,
            run_env,
        },
        |_| None,
    );
    let result = executor
        .execute(
            "execute_shell",
            &serde_json::json!({"command": "printenv", "args": [DECLARED_NAME]}),
        )
        .await;
    let stdout = result.data["stdout"].as_str().unwrap_or("").to_string();
    (result.success, stdout)
}

/// A declared variable reaches the real child of `execute_shell` through the production seam.
#[tokio::test]
async fn a_binding_declared_env_reaches_the_shell_child() {
    let run_env = BTreeMap::from([(DECLARED_NAME.to_string(), "binding-account-1".to_string())]);
    let (ok, stdout) = printenv_with_run_env(run_env).await;
    assert!(ok, "execute_shell should succeed");
    assert_eq!(
        stdout.trim(),
        "binding-account-1",
        "the declared binding env must reach the child"
    );
}

/// With no declaration the child sees nothing new (today's behaviour).
#[tokio::test]
async fn no_declaration_leaves_the_child_environment_unchanged() {
    let (ok, stdout) = printenv_with_run_env(BTreeMap::new()).await;
    assert!(ok, "execute_shell should succeed");
    assert_eq!(
        stdout, "",
        "an undeclared variable must not appear in the child"
    );
}
