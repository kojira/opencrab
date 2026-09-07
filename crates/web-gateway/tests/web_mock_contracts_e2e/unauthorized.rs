
// ==================== 契約 3: 未許可コマンド拒否 ====================

const M_SHELL: &str = "MARKERSHELL-run";

/// allowlist に無いコマンドの execute_shell が拒否され、拒否理由がエラー契約どおり
/// （"is not in the allowed list"）会話へ残る。
#[test]
fn unauthorized_shell_command_is_rejected() {
    // 初回は execute_shell(rm) を tool_call。拒否結果の再注入後は短い確認テキストで締める。
    let mock = spawn_mock(|req, _gate| {
        if has_tool_result(req) {
            return text_resp("rejected-ok 拒否を確認しました");
        }
        if req.contains(M_SHELL) {
            return tool_call_resp(
                "execute_shell",
                serde_json::json!({"command": "rm", "args": ["-rf", "/tmp/should-not-run-788"]}),
            );
        }
        text_resp("NO_REPLY")
    });
    // execute_shell は有効・allowlist は echo のみ（rm は未許可）。inline 実行のため auto_dispatch=false。
    let tools_block = "[tools]\nenabled = true\n\n[tools.shell]\nenabled = true\nallowed_commands = [\"echo\"]\ntimeout_secs = 30\n";
    let h = setup(mock.port, "shelldeny", false, tools_block);
    let db = &h.db;
    let ext_session = format!("extgate-{}", h.binding);

    post_message(
        h.gw_port,
        &h.session,
        "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        &format!("{M_SHELL} rm -rf を実行して"),
    );

    // 拒否理由がツール結果として再注入される（llm_logs.prompt に現れる）＝エラー契約の観測。
    let rejected = wait_until(Duration::from_secs(30), || {
        llm_prompt_hits(db, "is not in the allowed list") >= 1
    });
    assert!(
        rejected,
        "未許可コマンドの拒否（is not in the allowed list）が観測できない"
    );

    // 拒否理由には対象コマンド名 rm が入る（エラー契約の本文）。
    assert!(
        llm_prompt_hits(db, "Command 'rm' is not in the allowed list") >= 1,
        "拒否理由が契約どおりの本文（Command 'rm' is not in the allowed list）でない"
    );

    // 拒否後にターンが破綻せず締まる（継続ターンの確認 say が出る）。
    let closed = wait_until(Duration::from_secs(20), || {
        first_log_id(db, &ext_session, "rejected-ok").is_some()
    });
    assert!(closed, "拒否後の締めターンが出ない");
}
