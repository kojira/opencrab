use super::*;

/// この run で使う実行許可設定（`ToolsConfig`）を解決する。
///
/// グローバル設定（`AppState.tools_config`）の**複製**に、そのエージェントの DB 上の
/// 許可コマンドをマージして返す。グローバル側は決して書き換えない — 混ぜると
/// 全エージェントの許可が合流し、あるエージェントの許可が他へ漏れる（#202）。
/// 同じ方針は起動時の設定構築（`crate::main`）と REST の許可コマンド管理
/// （`crate::api::allowed_commands`）にも明文化されている。
///
/// **この関数が毎 run 無条件に呼ばれることが、許可コマンドツールがグローバル設定へ
/// 書き込む必要が無い理由**である（`crate::agent_management`）。DB が信頼できる
/// 情報源で、次の run はここで必ず拾い直す。
pub(crate) fn resolve_run_tools_config(
    state: &AppState,
    agent_id: &str,
) -> opencrab_actions::tools::ToolsConfig {
    let mut tools_cfg = state.tools_config.read().unwrap().clone();
    // DB 障害時は設定ファイル分だけで続行する（許可を落とす方向の degrade なので
    // run を止めるより安全）。ただし**黙って**落とさない: per-agent の許可が丸ごと
    // 消えたことは、ログが無いと誰も気づけない。本番の `Db` は r2d2 プールなので
    // 枯渇時の `lock()` は既定で 30 秒ブロックしてから `Err` になり得る。
    match state.db.lock() {
        Ok(conn) => match opencrab_db::queries::list_agent_allowed_commands(&conn, agent_id) {
            Ok(agent_cmds) => {
                if !agent_cmds.is_empty() {
                    let shell = tools_cfg
                        .shell
                        .get_or_insert_with(opencrab_actions::tools::ShellToolConfig::default);
                    for cmd in agent_cmds {
                        if !shell.allowed_commands.contains(&cmd) {
                            shell.allowed_commands.push(cmd);
                        }
                    }
                }
            }
            Err(e) => tracing::warn!(
                agent_id,
                error = %e,
                "per-agent 許可コマンドの取得に失敗。設定ファイル由来の許可だけで続行する\
                 （この run では追加分が効かない）"
            ),
        },
        Err(e) => tracing::warn!(
            agent_id,
            error = %e,
            "DB 接続の取得に失敗。設定ファイル由来の許可だけで続行する\
             （この run では per-agent の追加分が効かない）"
        ),
    }
    tools_cfg
}

/// Production の turn executor を組み立てるための wiring。
///
/// ここにある値は transport/run ごとの依存物だけで、caller/depth gate、system gateway
/// 合成、shell 登録、allowlist、MCP の depth-0 制限は [`build_turn_executor`] が決める。
pub(crate) struct TurnExecutorWiring {
    pub context: opencrab_actions::ActionContext,
    pub depth: u32,
    pub gateway_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>>,
    pub subtask_registry: opencrab_actions::SubtaskRegistry,
    pub completion_sink: Option<Arc<dyn opencrab_actions::SubtaskCompletionSink>>,
    pub subtask_starts: Option<Arc<std::sync::atomic::AtomicUsize>>,
    pub reply_target: Option<String>,
    pub tool_allowlist: Option<Vec<String>>,
}

/// Production と外形採取が共有する turn executor の唯一の組立口。
///
/// `mcp_provider` は接続済み provider の供給だけを抽象化する。信頼判定と depth-0 限定は
/// この関数内に残るため、呼び出し側が production と異なる注入条件を再実装できない。
pub(crate) fn build_turn_executor<F>(
    state: &AppState,
    wiring: TurnExecutorWiring,
    mcp_provider: F,
) -> opencrab_actions::BridgedExecutor
where
    F: FnOnce(bool) -> Option<Arc<dyn opencrab_gateway::GatewayActions>>,
{
    let mut dispatcher = opencrab_actions::ActionDispatcher::new();
    let tools_config = resolve_run_tools_config(state, &wiring.context.agent_id);
    opencrab_actions::register_tools_from_config(&tools_config, &mut dispatcher);

    let caller_is_trusted = matches!(
        wiring.context.caller,
        opencrab_actions::CallerIdentity::Owner
            | opencrab_actions::CallerIdentity::CoAgent { .. }
            | opencrab_actions::CallerIdentity::TrustedUser
    );
    let system_actions: Arc<dyn opencrab_gateway::GatewayActions> = Arc::new(
        crate::system_actions::SystemGatewayActions::new(
            state.clone(),
            wiring.gateway_actions,
            Some(wiring.subtask_registry),
            wiring.completion_sink,
        )
        .with_subtask_starts(wiring.subtask_starts),
    );
    let gateway_actions: Arc<dyn opencrab_gateway::GatewayActions> = if wiring.depth == 0 {
        system_actions
    } else {
        Arc::new(opencrab_actions::SubEngineGatewayActions::new(
            system_actions,
        ))
    };
    let bridged = opencrab_actions::BridgedExecutor::new(dispatcher, wiring.context)
        .with_depth(wiring.depth)
        .with_reply_target(wiring.reply_target)
        .with_tool_allowlist(wiring.tool_allowlist)
        .with_gateway_actions(gateway_actions);

    if wiring.depth == 0 {
        match mcp_provider(caller_is_trusted) {
            Some(provider) => bridged.with_mcp_actions(provider),
            None => bridged,
        }
    } else {
        bridged
    }
}

/// そのエージェントが `execute_shell` で**実際に実行できる**コマンド名の一覧（#300）。
///
/// [`resolve_run_tools_config`] の結果に `ShellToolConfig::effective_commands()` を
/// 掛けただけのもの。つまり LLM へ渡る `execute_shell` の `Allowed: ...` と
/// **同じ経路・同じ順序・同じ重複排除**で作られる。
///
/// 許可コマンド一覧ツール（`crate::agent_management::list_allowed_commands` /
/// `SystemGatewayActions::manage_allowed_commands` の `action="list"`）は
/// **必ずこの関数を通す**こと。ここを迂回して DB 行だけを返していたのが #300 の不具合で、
/// エージェントが「シェルは DB 行の 2 個しか使えない」と誤認して作業を止めた
/// （実際には設定ファイル由来の 10 個も同じターンで実行できていた）。
/// 出力先ごとに合成を書き直すと、片方だけ実態からずれて同じ誤認が再発する。
///
/// 一方、ダッシュボードの REST（`crate::api::allowed_commands::list_allowed_commands`）は
/// **この関数を通さない**。線引きは「LLM に露出する口 = 実効リスト / HTTP 管理 API =
/// DB 行」で、後者は add/remove と対の管理用ゆえに消せない行を混ぜてはならない。
///
/// DB 読み取りに失敗した場合は [`resolve_run_tools_config`] と同様に設定ファイル分だけを
/// 返す。**これは意図的**で、その run が実際に許可する集合と一致させるためである
/// （一覧だけがエラーを返しても、実行側は設定ファイル分を許可したまま動く）。
pub(crate) fn effective_allowed_commands(state: &AppState, agent_id: &str) -> Vec<String> {
    let tools_cfg = resolve_run_tools_config(state, agent_id);
    // 実際の run では `register_tools_from_config`（crates/actions/src/tools/mod.rs）が
    // `tools.enabled == false` なら **execute_shell 自体を登録しない**。ゲートを閉じた
    // 構成で許可コマンドを並べると「実行できないコマンド」を返し、エージェントが混乱する
    // （#311）。登録側と同じ条件でここも空へ倒す。register_tools_from_config が正で、
    // 一覧はそれに追従する。
    if !tools_cfg.enabled {
        return Vec::new();
    }
    tools_cfg
        .shell
        // 同様に shell 無し / `tools.shell.enabled == false` でも execute_shell は
        // 登録されない。`filter` で両方（None と enabled=false）を空へ落とす。
        .filter(|shell| shell.enabled)
        .map(|shell| {
            shell
                .effective_commands()
                .into_iter()
                .map(|c| c.name)
                .collect()
        })
        .unwrap_or_default()
}
