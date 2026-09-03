use super::build_agent_context;

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

    assert!(prompt.contains("## Silent Reply"), "prompt:\n{prompt}");

    // ループ防止は内容ベースで残る（#920: 事実文 §3.1 へ更新）。
    assert!(
        prompt.contains(
            "the topic is already resolved and a further exchange would add no new information"
        ),
        "content-based loop prevention was lost:\n{prompt}"
    );

    // 「相手が Bot だから黙る」という種別ベースの沈黙条件は消えていること。
    assert!(
        !prompt.contains("他のBotが話している場合"),
        "peer-type silence condition still present:\n{prompt}"
    );
}

/// #900 / #890 PR2（§11.2）: モデル向け契約を新アーキテクチャに合わせる。
/// 「result arrives later」はツール（query）起動の文脈にだけ現れ、発話は
/// (i) fire-and-forget（結果が返らない）・(ii) 複数は 1 応答に並べる・(iii) 末尾 CONTINUE で継続
/// （reply と併記可）、の 3 点を明示する。「## Continuing your turn」見出しと CONTINUE 指示も残る。
#[test]
fn system_prompt_explains_continue_marker() {
    let conn = opencrab_db::init_memory().unwrap();
    let (prompt, _name) =
        build_agent_context(&conn, "a1", &opencrab_actions::CallerIdentity::Owner);

    // 注: prompt の prose は各行末 `\n\` が文中に literal 改行を入れる house style の
    // ため、assert は 1 行内に収まる断片で確認する（長い文全体を連続一致で見ない）。
    assert!(
        prompt.contains("## Continuing your turn"),
        "継続マーカーの見出しが system prompt に無い:\n{prompt}"
    );
    assert!(
        prompt.contains("on its own line"),
        "CONTINUE をその行単独で置く指示が無い:\n{prompt}"
    );
    assert!(
        prompt.contains("`CONTINUE`"),
        "CONTINUE トークンの言及が無い:\n{prompt}"
    );
    // (i) 発話は fire-and-forget（結果が返らない）と明示する（#920: 事実文へ更新）。
    assert!(
        prompt.contains("fire-and-forget")
            && prompt.contains("return no result and you are not called again"),
        "発話が fire-and-forget（結果が返らない）である説明が無い:\n{prompt}"
    );
    // (ii) 複数の発話は 1 応答にまとめて並べる（#920: N 件なら N 呼び出しの事実文）。
    assert!(
        prompt.contains("N messages require N calls in this one response"),
        "複数発話を 1 応答に並べる説明が無い:\n{prompt}"
    );
    // (iii) CONTINUE は reply と併記できる（#920: sit alongside a reply）。
    assert!(
        prompt.contains("sit alongside a reply"),
        "CONTINUE を reply と併記できる説明が無い:\n{prompt}"
    );

    // 「result arrives later」はツール（query）起動の文脈にだけ現れる。旧「When you call a
    // tool, the result arrives later」という発話も含む包括表現は撤去されていること。
    assert!(
        prompt.contains("the result arrives later and you are called again"),
        "ツール結果が後で届く説明（query 文脈）が失われている:\n{prompt}"
    );
    assert!(
        !prompt.contains("When you call a tool, the result arrives later"),
        "旧・発話も含む包括的な非同期説明が残っている:\n{prompt}"
    );

    // #909: 平文は 1 応答 = 1 投稿。別々に複数投稿するなら各応答末尾 CONTINUE で次応答へ。
    // （#920: 「複数投稿はできない」を禁じる矯正文 Never say … は撤去済み・row 440。）
    assert!(
        prompt.contains("Plain text in a response is posted as ONE message"),
        "平文は 1 応答 = 1 投稿の説明が無い（#909）:\n{prompt}"
    );
    assert!(
        prompt.contains("To post several separate plain messages"),
        "別々の平文複数投稿の手順（各応答末尾 CONTINUE で次応答へ）の説明が無い（#909）:\n{prompt}"
    );
}
