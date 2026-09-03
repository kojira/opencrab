use super::build_agent_context;
use opencrab_db::queries::TrustedUserPermission;

/// 共有プロンプトから transport 語が消えていること（grep 相当をテスト化）。
#[test]
fn shared_prompt_has_no_transport_specific_terms() {
    let conn = opencrab_db::init_memory().unwrap();
    // #920: 名簿は Peer Review 節ごと prompt から撤去済み。レビュアーを登録しても
    // transport 語・名簿（表示名）が共有プロンプトに漏れないことを検査する。
    opencrab_db::queries::add_trusted_user(
        &conn,
        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
        "r1",
        "a1",
        "42",
        TrustedUserPermission::CoAgent,
        "owner",
        "2026-01-01",
        "Crab B",
    )
    .unwrap();

    let (prompt, _name) =
        build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

    // 空プロンプトを検査して通っているのではないことの canary（安定した節見出しで確認）。
    assert!(
        prompt.contains("## Silent Reply"),
        "prompt too small: {prompt}"
    );
    // #920: 登録レビュアーの表示名も共有プロンプトには出さない（名簿撤去）。
    assert!(
        !prompt.contains("Crab B"),
        "roster leaked into prompt: {prompt}"
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
