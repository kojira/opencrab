use super::super::*;
use super::support::*;

/// **DB に 1 行も無くても設定ファイル由来のコマンドが返る**（#300 の中核）。
///
/// 修正前はここが `[]` / `count: 0` になり、エージェントは「シェルは何も使えない」と
/// 読んだ。実際には設定ファイル分がそのまま実行できる。
#[tokio::test]
async fn list_allowed_commands_includes_config_base_without_any_db_row() {
    let state = state_with_shell_commands(&["ls", "cat", "grep"], &["python3"]);
    assert!(
        db_allowed_commands(&state, "agent-x").is_empty(),
        "前提: DB に per-agent の行は無い"
    );

    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute("list_allowed_commands", &json!({}), &agent_ctx())
        .await;
    assert!(r.success, "{:?}", r.error);
    let data = r.data.unwrap();
    assert_eq!(
        data["commands"],
        json!(["ls", "cat", "grep", "python3"]),
        "設定ファイル由来のコマンドが戻り値から落ちている（#300）"
    );
    // `count` は `commands` と必ず一致する（片方だけ古い値だと誤認の材料になる）。
    assert_eq!(data["count"], json!(4));
    assert_eq!(data["agent_id"], json!("agent-x"));
}

/// **設定ファイル分と DB 行が合成され、重複しない**。
///
/// 合成規則は `resolve_run_tools_config` +
/// `ShellToolConfig::effective_commands()` のものをそのまま使う:
/// 構造化 `commands` が先、その後ろに `allowed_commands`（設定 → DB の順）、
/// 既出の名前は積まない。`cargo` は設定と DB の両方にあるが 1 個だけ出る。
#[tokio::test]
async fn list_allowed_commands_merges_db_rows_with_config_without_duplicates() {
    let state = state_with_shell_commands(&["cargo"], &["ls", "cat"]);
    {
        let conn = state.db.lock().unwrap();
        for cmd in ["cargo", "mkdir"] {
            opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", cmd, "owner")
                .unwrap();
        }
    }
    assert_eq!(
        listed_commands(&state).await,
        vec!["cargo", "ls", "cat", "mkdir"],
        "設定 + DB の合成が実効リストと一致しない（#300）"
    );
}

/// **戻り値が `execute_shell` の `Allowed: ...` と 1 コマンドも食い違わない**。
///
/// #300 の実害はこの 2 つのズレそのもの。プロンプト側は正しく 12 個を並べていたのに
/// 一覧は 2 個しか返さず、エージェントは一覧を信じて止まった。両者を同じ解決点
/// （`process::effective_allowed_commands` / `resolve_run_tools_config`）から作る
/// 限りズレは起き得ないが、片方だけ書き換えられたらここで落ちる。
#[tokio::test]
async fn list_allowed_commands_matches_the_execute_shell_description() {
    let state = state_with_shell_commands(&["curl", "echo", "jq", "cargo"], &["python3"]);
    {
        let conn = state.db.lock().unwrap();
        for cmd in ["cargo", "mkdir"] {
            opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", cmd, "owner")
                .unwrap();
        }
    }

    // LLM に実際に渡る `execute_shell` の引数説明を、run と同じ手順で組み立てる。
    let shell_cfg = crate::process::resolve_run_tools_config(&state, "agent-x")
        .shell
        .expect("shell 設定");
    let shell_action = opencrab_actions::tools::shell::ShellToolAction::new(shell_cfg);
    let desc = opencrab_actions::Action::parameters(&shell_action)["properties"]["command"]
        ["description"]
        .as_str()
        .unwrap()
        .to_string();
    // このパースは `crates/actions/src/tools/shell.rs` の書式
    // （`... Allowed: a, b, c` で末尾がコマンド列）に依存している。
    let from_prompt: Vec<String> = desc
        .split_once("Allowed: ")
        .expect("`Allowed: ` を含む説明文（書式は crates/actions/src/tools/shell.rs）")
        .1
        .split(", ")
        .map(str::to_string)
        .collect();

    assert_eq!(
        listed_commands(&state).await,
        from_prompt,
        "一覧ツールの戻り値とプロンプトの Allowed が食い違っている（#300 の実害そのもの）。\
             ただし `Allowed: ` の後ろに文を足した場合もここが落ちる — 差分が末尾要素だけなら\
             まず `crates/actions/src/tools/shell.rs` の description 書式を確認し、\
             書式を変えたならこのパースを追随させること"
    );
}

/// owner 向けの `manage_allowed_commands(action="list")` も同じ実効リストを返す。
///
/// 一覧を返す口が 2 つあるので、片方だけ直すと「どちらを読んだか」で挙動が割れる。
#[tokio::test]
async fn manage_allowed_commands_list_returns_the_same_effective_list() {
    let state = state_with_shell_commands(&["cargo"], &["ls"]);
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::add_agent_allowed_command(&conn, "agent-x", "mkdir", "owner")
            .unwrap();
    }
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "manage_allowed_commands",
            &json!({"action": "list"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(r.data.unwrap()["commands"], json!(["cargo", "ls", "mkdir"]));
}

// ---- #311: 一覧は execute_shell の登録ゲートに追従する ----
//
// 実際の run は `register_tools_from_config`（crates/actions/src/tools/mod.rs）で
// `tools.enabled` と `tools.shell.enabled` の 2 段ゲートを通り、どちらかが false なら
// execute_shell 自体が登録されない。ゲートを閉じた構成で一覧が「実行できない
// コマンド」を返すと、エージェントが「使える」と誤認する（#311）。
// 下の 3 本はゲートが**空を返す**こと、4 本目はゲートが開いているとき従来どおり
// 返ること（恒真回避の対照）を固定する。ゲートを外すと下 3 本が赤くなる。

/// `tools.enabled = false` のとき一覧は空。
/// ゲートが無ければ `["ls", "cat"]` が返るので、空であることがゲートの証拠。
#[tokio::test]
async fn list_is_empty_when_tools_disabled() {
    let state = state_with_shell(&["ls", "cat"]);
    state.tools_config.write().unwrap().enabled = false;
    assert!(
        listed_commands(&state).await.is_empty(),
        "tools.enabled=false でも一覧が空になっていない（#311）"
    );
}

/// `tools.shell.enabled = false` のとき一覧は空（shell 設定はあるが無効）。
#[tokio::test]
async fn list_is_empty_when_shell_disabled() {
    let state = state_with_shell(&["ls", "cat"]);
    state
        .tools_config
        .write()
        .unwrap()
        .shell
        .as_mut()
        .unwrap()
        .enabled = false;
    assert!(
        listed_commands(&state).await.is_empty(),
        "tools.shell.enabled=false でも一覧が空になっていない（#311）"
    );
}

/// `shell` そのものが無い構成でも空（`tools.enabled=true` だが shell=None）。
#[tokio::test]
async fn list_is_empty_when_shell_absent() {
    let state = state_with_shell(&["ls"]);
    {
        let mut cfg = state.tools_config.write().unwrap();
        cfg.enabled = true;
        cfg.shell = None;
    }
    assert!(
        listed_commands(&state).await.is_empty(),
        "shell 設定が無いのに一覧が空でない（#311）"
    );
}

/// 両ゲートが true のときだけ従来どおり実効リストを返す（上の空テストが恒真でない対照）。
#[tokio::test]
async fn list_returns_commands_when_both_gates_enabled() {
    // state_with_shell の既定は enabled=true / shell.enabled=true。
    let state = state_with_shell(&["ls", "cat"]);
    assert_eq!(
        listed_commands(&state).await,
        vec!["ls", "cat"],
        "両ゲート true でも従来の実効リストが返らない（#311 のゲートが過剰）"
    );
}

/// owner 向けの `manage_allowed_commands(action="list")` も同じゲートに従う。
/// 一覧の口が 2 つあるので、片方だけ塞いで漏れないことを確かめる（#311）。
#[tokio::test]
async fn manage_allowed_commands_list_is_empty_when_tools_disabled() {
    let state = state_with_shell(&["ls", "cat"]);
    state.tools_config.write().unwrap().enabled = false;
    let actions = SystemGatewayActions::new(state.clone(), None, None, None);
    let r = actions
        .execute(
            "manage_allowed_commands",
            &json!({"action": "list"}),
            &owner_ctx(),
        )
        .await;
    assert!(r.success, "{:?}", r.error);
    assert_eq!(
        r.data.unwrap()["commands"],
        json!([]),
        "manage 経路が list 経路と食い違い、ゲートを漏れている（#311）"
    );
}
