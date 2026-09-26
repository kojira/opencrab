use super::prompt::build_agent_context;

/// 共有プロンプトから transport 語が消えていること（grep 相当をテスト化）。
#[test]
fn shared_prompt_has_no_transport_specific_terms() {
    let conn = opencrab_db::init_memory().unwrap();
    let (prompt, _name) =
        build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

    // 空プロンプトを検査して通っているのではないことの canary（安定した節見出しで確認）。
    assert!(
        prompt.contains("## Turn completion"),
        "prompt too small: {prompt}"
    );
    for needle in ["Discord", "discord", "[Discord context]", "<@"] {
        assert!(
            !prompt.contains(needle),
            "shared system prompt must not contain {needle:?}:\n{prompt}"
        );
    }
}

/// 宛先の取得方法を指示していないこと（宛先は実行側が文脈から既定値にする）。
#[test]
fn shared_prompt_does_not_teach_destination_lookup() {
    let conn = opencrab_db::init_memory().unwrap();
    let (prompt, _name) =
        build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);
    assert!(
        !prompt.contains("channel_id"),
        "shared system prompt must not name a transport destination argument:\n{prompt}"
    );
    // #920: Peer Review 節（宛先の説明を含む）は撤去済み。宛先取得を教える語が無いこと。
    assert!(
        !prompt.contains("destination"),
        "shared system prompt must not teach destination lookup:\n{prompt}"
    );
}
