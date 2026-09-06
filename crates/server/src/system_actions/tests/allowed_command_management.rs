use super::super::*;
use super::support::*;
use opencrab_gateway::GatewayCaller;

/// **#157 S1 の本題**: 4 ツールが `SystemGatewayActions` の own 定義になっている。
///
/// own 定義は transport の有無に依存しないため、これが `definitions()` に出ることは
/// 「web / Nostr / REST / heartbeat でも使える」ことと同義である。own から消えると
/// Discord 専用に逆戻りする（それが #157 が報告している不具合そのもの）。
#[test]
fn generic_management_tools_are_exposed_in_own_definitions() {
    let defs = SystemGatewayActions::own_definitions();
    for name in [
        "update_memory_index_config",
        "add_allowed_command",
        "list_allowed_commands",
        "remove_allowed_command",
    ] {
        assert_eq!(
            defs.iter().filter(|d| d.name == name).count(),
            1,
            "{name} は own 定義にちょうど 1 件必要（#157 S1）"
        );
    }
}

/// **Discord 無効の構成でも 4 ツールが露出する**（#157 S1 の証明）。
///
/// `inner = None` は「transport 固有 gateway が居ない」経路（web / REST /
/// heartbeat、および Discord feature 無効ビルド）そのもの。移設前はこの構成で
/// 4 ツールが一切出なかった。
#[test]
fn generic_management_tools_are_exposed_without_any_transport_gateway() {
    let state = crate::test_app_state();
    let actions = SystemGatewayActions::new(state, None, None, None);
    let names: Vec<String> = actions.definitions().into_iter().map(|d| d.name).collect();
    for name in [
        "update_memory_index_config",
        "add_allowed_command",
        "list_allowed_commands",
        "remove_allowed_command",
    ] {
        assert!(
                names.contains(&name.to_string()),
                "transport gateway 無しの構成で {name} が露出しない（#157 の不具合そのもの）: {names:?}"
            );
    }
}

/// 引数スキーマを移設前（Discord 定義）と同一に保つ。
#[test]
fn generic_management_tool_schemas_match_the_discord_originals() {
    let defs = SystemGatewayActions::own_definitions();
    let find = |n: &str| defs.iter().find(|d| d.name == n).unwrap();

    let d = find("update_memory_index_config");
    assert!(d.parameters["required"].as_array().unwrap().is_empty());
    let props = d.parameters["properties"].as_object().unwrap();
    assert_eq!(props["batch_size"]["type"], json!("integer"));
    assert_eq!(props["threshold"]["type"], json!("integer"));

    for n in ["add_allowed_command", "remove_allowed_command"] {
        let d = find(n);
        assert_eq!(d.parameters["required"], json!(["command"]), "{n}");
        assert_eq!(
            d.parameters["properties"]["command"]["type"],
            json!("string"),
            "{n}"
        );
    }

    let d = find("list_allowed_commands");
    assert!(d.parameters["required"].as_array().unwrap().is_empty());
    assert!(d.parameters["properties"].as_object().unwrap().is_empty());
}

/// **オーナー限定検査が移設後も効く**（add）。
///
/// #330 以降は多層防御になった: bridge policy 層（`OWNER_ONLY_ACTIONS`）でも
/// `add_allowed_command` / `remove_allowed_command` を owner_only にゲートし、加えて
/// この server ハンドラ内検査も残る。SystemGatewayActions を直接叩くこのテストは
/// bridge 層を通らないので、ハンドラ側の owner 検査が単独で効くことを固定する。
///
/// エラー文言はバイト単位で移設前と同一（移設で文言が変わっていないことの防波堤）。
#[tokio::test]
async fn add_allowed_command_rejects_non_owner_without_side_effects() {
    // bridge policy 層でも owner_only であること（#330 の多層防御）。
    assert!(
        opencrab_actions::OWNER_ONLY_ACTIONS.contains(&"add_allowed_command"),
        "#330: add_allowed_command は bridge policy 層でも owner_only であるべき"
    );

    let state = state_with_shell(&[]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &agent_ctx(),
        )
        .await;

    assert!(!r.success);
    assert_eq!(
        r.error.as_deref(),
        Some("このアクションはオーナーのみ実行できます"),
        "拒否文言は移設前と 1 文字も変えない"
    );
    assert!(r.data.is_none());
    // 副作用ゼロ: DB も走行中の実行許可設定も変わらない。
    assert!(db_allowed_commands(&state, "agent-x").is_empty());
    assert!(live_allowed_commands(&state).is_empty());
}

/// **オーナー限定検査が移設後も効く**（remove）。既に許可済みのコマンドが
/// 非オーナーの呼び出しで消えないこと。
#[tokio::test]
async fn remove_allowed_command_rejects_non_owner_without_side_effects() {
    // bridge policy 層でも owner_only であること（#330 の多層防御）。
    assert!(
        opencrab_actions::OWNER_ONLY_ACTIONS.contains(&"remove_allowed_command"),
        "#330: remove_allowed_command は bridge policy 層でも owner_only であるべき"
    );

    let state = state_with_shell(&["git"]);
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "git", "owner").unwrap();
    }
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "remove_allowed_command",
            &json!({"command": "git"}),
            &agent_ctx(),
        )
        .await;

    assert!(!r.success);
    assert_eq!(
        r.error.as_deref(),
        Some("このアクションはオーナーのみ実行できます"),
        "拒否文言は移設前と 1 文字も変えない"
    );
    // 許可は残っている（DB も走行中の設定も）。
    assert_eq!(db_allowed_commands(&state, "agent-x"), vec!["git"]);
    assert_eq!(live_allowed_commands(&state), vec!["git"]);
}

/// **グローバルな実行許可設定へは書かない**（#202）。DB だけが更新される。
///
/// 移設前の Discord 実装は DB と併せてグローバル設定にも書き込んでいた。応答生成は
/// **全エージェント**についてこの設定を実行許可の土台として複製する
/// （`crate::process::resolve_run_tools_config`）ので、その書き込みは
/// 「A が許可したコマンドが全エージェントで実行可能になる」漏れそのものだった。
///
/// このテストは**旧 `add_allowed_command_updates_the_live_shared_tools_config` の
/// 反転**である。旧テストは漏れを不変条件として固定していた。
#[tokio::test]
async fn add_allowed_command_does_not_write_to_the_global_tools_config() {
    let state = state_with_shell(&["ls"]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    // DB へ永続化されている（信頼できる情報源）。
    assert_eq!(db_allowed_commands(&state, "agent-x"), vec!["curl"]);
    // グローバル設定は 1 文字も変わらない。
    assert_eq!(
        live_allowed_commands(&state),
        vec!["ls"],
        "グローバル設定へ書き込むと全エージェントへ漏れる（#202）"
    );
}

/// 削除もグローバル設定を触らない（追加と対称 / #202）。
///
/// 旧実装は `retain` でグローバル設定からも消していたため、**設定ファイル由来の
/// コマンドをエージェントの操作でグローバルに削除できた**。
/// 旧 `remove_allowed_command_updates_the_live_shared_tools_config` の反転。
#[tokio::test]
async fn remove_allowed_command_does_not_write_to_the_global_tools_config() {
    // "curl" は**設定ファイル由来**でもあり、かつ agent-x の DB 許可でもある状態。
    let state = state_with_shell(&["ls", "curl"]);
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "curl", "owner").unwrap();
    }
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "remove_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    assert!(db_allowed_commands(&state, "agent-x").is_empty());
    assert_eq!(
        live_allowed_commands(&state),
        vec!["ls", "curl"],
        "設定ファイル由来のコマンドをエージェントの操作で消してはならない（#202）"
    );
}

/// **エージェント A の追加が、エージェント B の実行許可を変えない**（#202 の本体）。
///
/// 「次の run が何を許可するか」は `crate::process::resolve_run_tools_config` が
/// 決める（応答生成が毎 run 呼ぶ唯一の解決点）。A の追加後にそれを両エージェントで
/// 解決し、A にだけ効いていることを固定する。
///
/// これが `add_allowed_command_takes_effect_on_the_next_run_but_not_the_same_turn` と対になって
/// 「撤去しても呼び出し元は困らない / 他エージェントへは漏れない」の両方を証明する。
#[tokio::test]
async fn add_allowed_command_does_not_change_another_agents_permissions() {
    let state = state_with_shell(&["ls"]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &GatewayCallContext::new(GatewayCaller::Owner, "agent-a"),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    assert_eq!(
        run_allowed_commands(&state, "agent-a"),
        vec!["ls", "curl"],
        "追加したエージェント自身には次の run で効かなければならない"
    );
    assert_eq!(
        run_allowed_commands(&state, "agent-b"),
        vec!["ls"],
        "agent-a の追加が agent-b の実行許可を広げてはならない（#202）"
    );
    // グローバル設定そのものも汚れていない。
    assert_eq!(live_allowed_commands(&state), vec!["ls"]);
}

/// **エージェント A の削除が、設定ファイル由来のコマンドや B の許可を消さない**（#202）。
#[tokio::test]
async fn remove_allowed_command_does_not_change_another_agents_permissions() {
    // 設定ファイル由来: "ls"。A と B の両方が DB で "curl" を許可されている。
    let state = state_with_shell(&["ls"]);
    {
        let conn = state.db.lock().unwrap();
        for agent in ["agent-a", "agent-b"] {
            opencrab_db::queries::add_agent_allowed_command(&conn, agent, "curl", "owner").unwrap();
        }
    }
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    let r = actions
        .execute(
            "remove_allowed_command",
            &json!({"command": "curl"}),
            &GatewayCallContext::new(GatewayCaller::Owner, "agent-a"),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    assert_eq!(
        run_allowed_commands(&state, "agent-a"),
        vec!["ls"],
        "削除は呼び出したエージェントには次の run で効く"
    );
    assert_eq!(
        run_allowed_commands(&state, "agent-b"),
        vec!["ls", "curl"],
        "agent-a の削除が agent-b の許可を消してはならない（#202）"
    );
    assert_eq!(
        live_allowed_commands(&state),
        vec!["ls"],
        "設定ファイル由来の許可は残る（#202）"
    );
}

/// **追加した許可は「次の run」で呼び出したエージェントに効く**（撤去の前提の実証）。
///
/// グローバル設定への書き込みを撤去してよい根拠は 2 つあり、両方をここで実際に
/// 走らせて確かめる。
///
/// 1. **次の run で効く**: run の冒頭で `resolve_run_tools_config` が DB の許可を
///    ローカル複製へマージし、`register_tools_from_config` がそれを `ShellToolAction`
///    へ渡す。したがって次の run のシェルツールは許可リスト検査を通す。
/// 2. **同ターンでは元から効かない**: ツール登録は run 冒頭のスナップショットなので、
///    許可を追加しても**その run で登録済みのツール**には届かない。つまりグローバル
///    書き込みを撤去しても失われる機能は無い（撤去前も同ターン反映は無かった）。
///
/// 許可リスト検査だけを見るため、実際には存在しないコマンド名を使う。
/// 拒否は「allowed list に無い」/ 通過は「spawn 失敗」で区別でき、プロセスは
/// 一切起動しない（PATH や OS 差に依存しない）。
#[tokio::test]
async fn add_allowed_command_takes_effect_on_the_next_run_but_not_the_same_turn() {
    const CMD: &str = "opencrab_absent_probe";

    let state = state_with_shell(&[]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    // --- この run のツールを登録する（run 冒頭のスナップショット） ---
    let mut this_run = opencrab_actions::ActionDispatcher::new();
    opencrab_actions::register_tools_from_config(
        &crate::process::resolve_run_tools_config(&state, "agent-x"),
        &mut this_run,
    );

    // --- 走行中に許可を追加する ---
    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": CMD}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);

    let (_dir, ctx) = shell_ctx();

    // 根拠 2: **同ターンでは効かない**（登録済みツールはスナップショットを持つ）。
    let same_turn = this_run
        .execute("execute_shell", &json!({"command": CMD}), &ctx)
        .await;
    assert!(!same_turn.success);
    assert!(
        same_turn
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("is not in the allowed list"),
        "同ターン反映は元から効かない前提が崩れている: {:?}",
        same_turn.error
    );

    // 根拠 1: **次の run では効く**（DB からマージされる）。
    let mut next_run = opencrab_actions::ActionDispatcher::new();
    opencrab_actions::register_tools_from_config(
        &crate::process::resolve_run_tools_config(&state, "agent-x"),
        &mut next_run,
    );
    let next = next_run
        .execute("execute_shell", &json!({"command": CMD}), &ctx)
        .await;
    assert!(!next.success, "存在しないコマンドなので spawn は失敗する");
    let e = next.error.as_deref().unwrap_or_default();
    assert!(
        !e.contains("is not in the allowed list"),
        "次の run では許可リスト検査を通らなければならない（撤去の前提）: {e}"
    );
    assert!(
        e.contains("Failed to spawn command"),
        "許可リストを通過して spawn まで到達したはず: {e}"
    );

    // グローバル設定は最後まで汚れていない。
    assert!(live_allowed_commands(&state).is_empty());
}

/// **コマンド名の文字種検査が効く**（英数字・`-`・`_` のみ）。
///
/// 同系統の `manage_allowed_commands` は trim だけなので、移設でこちらを緩めると
/// `rm -rf /` のようなシェル片やパス区切りを許可リストへ入れられてしまう。
/// 検査は DB へ触る**前**に行う（副作用ゼロ）。
#[tokio::test]
async fn add_allowed_command_rejects_invalid_command_characters() {
    let state = state_with_shell(&[]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    for bad in ["rm -rf /", "/bin/sh", "git;whoami", "cat|less", "a$b"] {
        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": bad}),
                &owner_ctx(),
            )
            .await;
        assert!(!r.success, "{bad} は拒否されなければならない");
        let e = r.error.unwrap();
        assert_eq!(
                e,
                format!(
                    "コマンド名に無効な文字が含まれています: {}（英数字・ハイフン・アンダースコアのみ使用可）",
                    bad
                ),
                "文字種エラーの文言は移設前と同一"
            );
    }
    // 1 件も通っていない。
    assert!(db_allowed_commands(&state, "agent-x").is_empty());
    assert!(live_allowed_commands(&state).is_empty());

    // 対: 妥当な文字（英数字・ハイフン・アンダースコア）は通る。
    for good in ["curl", "docker-compose", "my_tool", "python3"] {
        let r = actions
            .execute(
                "add_allowed_command",
                &json!({"command": good}),
                &owner_ctx(),
            )
            .await;
        assert!(r.success, "{good} は許可されるべき: {:?}", r.error);
    }
}

/// `command` 未指定 / 空文字は移設前と同じ文言で失敗する（add / remove の両方）。
#[tokio::test]
async fn allowed_command_tools_require_a_non_empty_command() {
    let state = state_with_shell(&[]);
    let actions = SystemGatewayActions::new(state, None, None, None);
    for name in ["add_allowed_command", "remove_allowed_command"] {
        for args in [json!({}), json!({"command": ""}), json!({"command": 42})] {
            let r = actions.execute(name, &args, &owner_ctx()).await;
            assert!(!r.success, "{name} {args}");
            assert_eq!(
                r.error.as_deref(),
                Some("commandパラメータが必要です"),
                "{name} {args}"
            );
        }
    }
}

/// **レスポンス JSON が移設前と同一**（許可コマンド 3 種）。期待値をリテラルで固定する。
#[tokio::test]
async fn allowed_command_response_json_is_unchanged() {
    let state = state_with_shell(&[]);
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);

    // 追加（新規）
    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "command": "curl",
            "agent_id": "agent-x",
            "message": "`curl` を許可コマンドに追加しました",
        })
    );

    // 追加（既存）: `already_exists` が付く。
    let r = actions
        .execute(
            "add_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "command": "curl",
            "agent_id": "agent-x",
            "message": "`curl` はすでに許可コマンドに登録されています",
            "already_exists": true,
        })
    );

    // 一覧: commands / count / agent_id の 3 キー。
    let r = actions
        .execute("list_allowed_commands", &json!({}), &agent_ctx())
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "commands": ["curl"],
            "count": 1,
            "agent_id": "agent-x",
        })
    );

    // 削除（存在した）
    let r = actions
        .execute(
            "remove_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "command": "curl",
            "agent_id": "agent-x",
            "message": "`curl` を許可コマンドから削除しました",
        })
    );

    // 削除（存在しない）: `not_found` が付き、success は true のまま。
    let r = actions
        .execute(
            "remove_allowed_command",
            &json!({"command": "curl"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success);
    assert_eq!(
        r.data.unwrap(),
        json!({
            "command": "curl",
            "agent_id": "agent-x",
            "message": "`curl` は許可コマンドに登録されていませんでした",
            "not_found": true,
        })
    );
}

/// 一覧は**呼び出し元のエージェント**の許可コマンドだけを返す（agent_id スコープ）。
///
/// `state_with_shell(&[])` を使うのは意図的: 生の `test_app_state()` は
/// `ToolsConfig::default()`（`enabled: false`）で、#311 のゲート追加後は一覧が
/// 空になってしまう。ここで検証したいのは agent_id スコープであってゲートではないので、
/// ゲートを開いた（`enabled: true` / `shell.enabled: true`）最小構成に載せ替える。
#[tokio::test]
async fn list_allowed_commands_is_scoped_to_the_calling_agent() {
    let state = state_with_shell(&[]);
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "curl", "owner").unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, "other-agent", "wget", "owner")
            .unwrap();
    }
    let actions = SystemGatewayActions::new(state, None, None, None);
    let r = actions
        .execute("list_allowed_commands", &json!({}), &agent_ctx())
        .await;
    assert!(r.success);
    assert_eq!(r.data.unwrap()["commands"], json!(["curl"]));
}
