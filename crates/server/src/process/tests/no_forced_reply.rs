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

    // 継続可否は相手の種別ではなく、モデル自身が現在のターンごとに判断する。
    assert!(
        prompt.contains("Decide whether to continue the current turn"),
        "turn-level continuation decision was lost:\n{prompt}"
    );

    // 「相手が Bot だから黙る」という種別ベースの沈黙条件は消えていること。
    assert!(
        !prompt.contains("他のBotが話している場合"),
        "peer-type silence condition still present:\n{prompt}"
    );
}

/// 発話の有無と継続可否をモデルが判断し、NO_REPLY で終了を示す契約を伝える。
#[test]
fn system_prompt_explains_explicit_termination() {
    let conn = opencrab_db::init_memory().unwrap();
    let (prompt, _name) =
        build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

    assert!(prompt.contains("## Turn completion"), "prompt:\n{prompt}");
    assert!(
        prompt.contains("Decide whether to continue the current turn"),
        "継続判断の説明が無い:\n{prompt}"
    );
    assert!(
        prompt.contains("If no speech should be delivered, respond with exactly `NO_REPLY`"),
        "無発話終了の説明が無い:\n{prompt}"
    );
    assert!(
        prompt.contains("If you provide speech and decide to end the turn, append `NO_REPLY`"),
        "発話後の終了方法が無い:\n{prompt}"
    );
    assert!(
        prompt.contains("If you decide to continue the turn, omit `NO_REPLY`"),
        "継続方法が無い:\n{prompt}"
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
