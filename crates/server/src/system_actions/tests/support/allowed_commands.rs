use super::super::super::*;
use super::call_contexts::agent_ctx;

// ---- #157 S1: 汎用管理ツール 4 個の gateway 非依存化 ----
//
// 移設前（origin/main）にはこの 4 ツールの挙動テストが**1 件も無かった**ため、
// ここは「移植」ではなく新規に契約を覆うテスト群である。守っている不変条件は
// `crate::agent_management` のモジュール doc に列挙してある。

/// 実行許可設定に shell セクションを持たせた `AppState`。
///
/// `initial` は**設定ファイル由来**の許可コマンド（グローバル設定）を模す。
/// per-agent の許可（DB）と混ざらないこと（#202）を検証するには、この 2 つが
/// 区別できる構成が必要。
pub(crate) fn state_with_shell(initial: &[&str]) -> AppState {
    let state = crate::test_app_state();
    {
        let mut cfg = state.tools_config.write().unwrap();
        cfg.enabled = true;
        cfg.shell = Some(opencrab_actions::tools::ShellToolConfig {
            enabled: true,
            allowed_commands: initial.iter().map(|s| s.to_string()).collect(),
            timeout_secs: 30,
            max_timeout_secs: 300,
            working_dir: None,
            inherit_env: false,
            allowed_env_vars: Vec::new(),
            max_output_bytes: 1024,
            commands: Vec::new(),
        });
    }
    state
}

/// 走行中の実行許可設定（`AppState.tools_config`）に載っているコマンド一覧。
pub(crate) fn live_allowed_commands(state: &AppState) -> Vec<String> {
    state
        .tools_config
        .read()
        .unwrap()
        .shell
        .as_ref()
        .map(|s| s.allowed_commands.clone())
        .unwrap_or_default()
}

/// DB に永続化されている許可コマンド一覧。
pub(crate) fn db_allowed_commands(state: &AppState, agent_id: &str) -> Vec<String> {
    let conn = state.db.lock().unwrap();
    opencrab_db::queries::list_agent_allowed_commands(&conn, agent_id).unwrap()
}

/// **次の run** がそのエージェントに許可するコマンド一覧。
///
/// 応答生成（`crate::process`）が毎 run 呼ぶ解決点をそのまま使う。グローバル設定と
/// 混同しないよう、per-agent の実効値はこのヘルパー越しにだけ見る。
pub(crate) fn run_allowed_commands(state: &AppState, agent_id: &str) -> Vec<String> {
    crate::process::resolve_run_tools_config(state, agent_id)
        .shell
        .map(|s| s.allowed_commands)
        .unwrap_or_default()
}

/// シェルツールを実際に dispatch するための `ActionContext`（作業ディレクトリ付き）。
pub(crate) fn shell_ctx() -> (tempfile::TempDir, opencrab_actions::ActionContext) {
    let dir = tempfile::TempDir::new().unwrap();
    let ws = opencrab_core::workspace::Workspace::from_root(dir.path()).unwrap();
    let conn = opencrab_db::init_memory().unwrap();
    let ctx = opencrab_actions::ActionContext {
        caller: opencrab_actions::CallerIdentity::Owner,
        agent_id: "agent-x".to_string(),
        agent_name: "Agent X".to_string(),
        session_id: None,
        db: opencrab_db::Db::from_connection(conn),
        workspace: Arc::new(ws),
        last_metrics_id: Arc::new(std::sync::Mutex::new(None)),
        model_override: Arc::new(std::sync::Mutex::new(None)),
        current_purpose: Arc::new(std::sync::Mutex::new("test".to_string())),
        runtime_info: Arc::new(std::sync::Mutex::new(opencrab_actions::RuntimeInfo {
            default_model: "mock:test".to_string(),
            active_model: None,
            available_providers: vec![],
            gateway: "test".to_string(),
        })),
    };
    (dir, ctx)
}

// ---- #300: 一覧が「実効リスト」であること ----
//
// 不具合そのものは「`list_allowed_commands` が DB 行しか返さず、設定ファイル由来の
// コマンドが落ちる」。エージェントは戻り値を「これが使える全部だ」と読むので、
// 落ちた分は「使えない」と誤認され、実際には実行できる作業が止まった。

/// 設定ファイル相当の shell 設定（構造化 `commands` + 素の `allowed_commands`）を
/// 持つ `AppState`。実運用の `config/default.toml` は `[[tools.shell.commands]]`
/// （構造化）で 10 個を与えるので、そちら側も再現できないと #300 を覆えない。
pub(crate) fn state_with_shell_commands(structured: &[&str], plain: &[&str]) -> AppState {
    let state = state_with_shell(plain);
    {
        let mut cfg = state.tools_config.write().unwrap();
        let shell = cfg.shell.as_mut().unwrap();
        shell.commands = structured
            .iter()
            .map(|name| opencrab_actions::tools::config::CommandConfig {
                name: name.to_string(),
                permission: opencrab_actions::tools::config::CommandPermission::Agent,
                timeout_secs: None,
                description: None,
            })
            .collect();
    }
    state
}

pub(crate) async fn listed_commands(state: &AppState) -> Vec<String> {
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute("list_allowed_commands", &json!({}), &agent_ctx())
        .await;
    assert!(r.success, "{:?}", r.error);
    serde_json::from_value(r.data.unwrap()["commands"].clone()).unwrap()
}
