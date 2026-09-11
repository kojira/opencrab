use super::prompt::build_agent_context;

#[test]
fn the_prompt_does_not_force_a_reply() {
    let conn = opencrab_db::init_memory().unwrap();
    let (prompt, _name) =
        build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

    for forbidden in [
        "最優先の例外",
        "人間（Bot ではない送信者）があなたに宛てて発言した場合は",
        "If a human spoke to you after your last message",
        "This rule wins over 3.",
    ] {
        assert!(
            !prompt.contains(forbidden),
            "#288 の強制文言が残っている: {forbidden}"
        );
    }
}

/// ループ防止（Silent Reply の元の意図）は残るが、判断は相手の種別ではなく会話内容で
/// 行わせる（#486・理念: システムは相手が bot か判定しない）。
#[test]
fn loop_prevention_survives_but_not_by_peer_type() {
    let conn = opencrab_db::init_memory().unwrap();
    let (prompt, _name) =
        build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

    assert!(prompt.contains("## Turn completion"), "prompt:\n{prompt}");

    // ループ防止は内容ベースで残る（#920: 事実文 §3.1 へ更新）。
    assert!(
        prompt.contains(
            "a topic that is already resolved where another exchange would add no new information"
        ),
        "content-based loop prevention was lost:\n{prompt}"
    );

    // 「相手が Bot だから黙る」という種別ベースの沈黙条件は消えていること。
    assert!(
        !prompt.contains("他のBotが話している場合"),
        "peer-type silence condition still present:\n{prompt}"
    );
}

/// ターンは既定継続で、NO_REPLY だけが明示終端になる契約をモデルへ伝える。
#[test]
fn system_prompt_explains_explicit_termination() {
    let conn = opencrab_db::init_memory().unwrap();
    let (prompt, _name) =
        build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

    assert!(prompt.contains("## Turn completion"), "prompt:\n{prompt}");
    assert!(
        prompt.contains("Your turn continues by default"),
        "既定継続の説明が無い:\n{prompt}"
    );
    assert!(
        prompt.contains("Only `NO_REPLY` explicitly ends it"),
        "明示終端の説明が無い:\n{prompt}"
    );
    assert!(
        prompt.contains("marker is recorded as a turn-termination event"),
        "終了記録を保存する説明が無い:\n{prompt}"
    );
    assert!(
        prompt.contains("fire-and-forget")
            && prompt.contains("do not end the turn unless you also write `NO_REPLY`"),
        "発話後も既定継続する説明が無い:\n{prompt}"
    );
    assert!(
        prompt.contains("the result arrives later and you are called again"),
        "background result の説明が失われた:\n{prompt}"
    );
}
