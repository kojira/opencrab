// ============ (#899) NO_REPLY のみの応答は speech として保存しない（extgate 境界） ============

/// #899: 配送層（extgate V3）で `NO_REPLY` のみの応答が沈黙になっても、`content='NO_REPLY'`
/// の speech 行が memory_sessions に残り、次ターンの typed 履歴で `assistant: 'NO_REPLY'` として
/// モデルへ渡っていた（`apply_delivery_effect` の NoReply 分岐が `record_agent_no_reply` を
/// 呼んで沈黙マーカーを永続していた）。
///
/// 期待（テンプレ §1・観測境界＝ゲート配送回数/本文・memory_sessions 保存件数/本文・typed 履歴）:
///
/// | シナリオ            | say 配送 | agent speech 保存        | typed の assistant |
/// |---------------------|----------|--------------------------|--------------------|
/// | (a) `NO_REPLY` のみ | 0        | 0（NO_REPLY 行を残さない）| NO_REPLY 無し      |
/// | (b) 本文+`NO_REPLY` | 1（本文）| 1（本文のみ・NO_REPLY 無）| 本文のみ           |
/// | (c) 対照: 通常応答  | 1        | 1                        | 通常応答           |
///
/// (a) が現 tip で赤（`NO_REPLY` 行が保存され typed に現れる）。
#[tokio::test]
async fn scenario_no_reply_only_is_not_persisted_extgate_899() {
    // 一意マーカー（グローバル say バッファの他テスト混線回避）。
    const BODY_B: &str = "NR899B-本文だけ残る";
    const CTRL_C: &str = "NR899C-通常応答";

    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    // FIFO: (a) 単独NO_REPLY → (b) 本文+NO_REPLY → (d) 終端後の破棄対象 → (c) 対照兼バリア。
    mock.push_text("NO_REPLY");
    mock.push_text(&format!("{BODY_B}\nNO_REPLY"));
    mock.push_text("NO_REPLY\n破棄対象");
    mock.push_text(&format!("{CTRL_C}\nNO_REPLY"));
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    // 4 メンションを順に投入（同一 binding＝同一セッション・consumer が FIFO 直列化）。
    // (d) は終端後の文字列を破棄して沈黙する。
    fixture.append_line(&mention_event(&"a1".repeat(32), "NR899-mention-a"));
    fixture.append_line(&mention_event(&"b2".repeat(32), "NR899-mention-b"));
    fixture.append_line(&mention_event(&"d4".repeat(32), "NR899-mention-d"));
    fixture.append_line(&mention_event(&"c3".repeat(32), "NR899-mention-c"));

    // バリア: 対照(c)の say が出れば FIFO 上 (a)(b) のターンは決着済み。
    let ok = {
        let buf = buf.clone();
        wait_until(move || captured(&buf).iter().any(|s| s.body.contains(CTRL_C))).await
    };
    assert!(ok, "対照(c)の say が出ない: {:?}", captured(&buf));

    // --- 観測1: ゲート配送（dry-run say） ---
    let says = captured(&buf);
    // (a) 単独 NO_REPLY はどの say にも本文が無い＝配送 0。どの say body にも NO_REPLY が混入しない。
    for s in &says {
        assert!(
            !s.body.contains("NO_REPLY"),
            "say body に NO_REPLY が混入（配送層剥がし漏れ）: {:?}",
            s
        );
    }
    // (b) 本文のみ配送 1・(c) 配送 1。
    assert_eq!(
        says.iter().filter(|s| s.body.contains(BODY_B)).count(),
        1,
        "(b) 本文の say が 1 回配送されていない: {:?}",
        says
    );
    assert_eq!(
        says.iter().filter(|s| s.body.contains(CTRL_C)).count(),
        1,
        "(c) 対照の say が 1 回配送されていない: {:?}",
        says
    );

    // --- 観測2: memory_sessions の agent speech 保存 ---
    let agent_speech: Vec<String> = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_db::queries::list_session_logs_by_session(&conn, &session_id)
            .unwrap()
            .into_iter()
            .filter(|l| l.log_type == "speech" && l.speaker_id.as_deref() == Some(AGENT_ID))
            .map(|l| l.content)
            .collect()
    };
    // (a) NO_REPLY のみの生成は speech を残さない。
    assert_eq!(
        agent_speech
            .iter()
            .filter(|c| c.contains("NO_REPLY"))
            .count(),
        0,
        "NO_REPLY を含む agent speech 行が残っている（#899）: {:?}",
        agent_speech
    );
    // (b) 本文だけ 1 行（NO_REPLY 無し）。
    let b_rows: Vec<&String> = agent_speech.iter().filter(|c| c.contains(BODY_B)).collect();
    assert_eq!(
        b_rows.len(),
        1,
        "(b) 本文の保存が 1 行でない: {:?}",
        agent_speech
    );
    assert!(
        !b_rows[0].contains("NO_REPLY"),
        "(b) 保存本文に NO_REPLY が混入: {:?}",
        b_rows[0]
    );
    // (c) 対照 1 行。
    assert_eq!(
        agent_speech.iter().filter(|c| c.contains(CTRL_C)).count(),
        1,
        "(c) 対照の保存が 1 行でない: {:?}",
        agent_speech
    );

    // --- 観測3: 次ターンの typed 履歴に assistant 'NO_REPLY' が無い ---
    let history = {
        let conn = core.extgate.db.lock().unwrap();
        opencrab_core::conversation_typed::build_typed_conversation(
            &conn,
            &session_id,
            AGENT_ID,
            200_000,
            100_000,
            false,
            false,
        )
        .unwrap()
        .history
    };
    let assistant_no_reply = history.iter().any(|m| {
        m.role == Role::Assistant
            && m.text_content()
                .map(|t| t.trim() == "NO_REPLY")
                .unwrap_or(false)
    });
    assert!(
        !assistant_no_reply,
        "typed 履歴に assistant 'NO_REPLY' が現れた（#899）: {:?}",
        history
            .iter()
            .map(|m| (
                format!("{:?}", m.role),
                m.text_content().map(|s| s.to_string())
            ))
            .collect::<Vec<_>>()
    );

    // --- 観測4: 各ターンの LLM 呼び出しは 1 回（ターン合計 noreply: LLM==1）---
    // 4 メンション＝4 ターン。各ターンが plain text で 1 生成のみ（沈黙 a/d も含め再生成しない）。
    assert_eq!(
        mock.system_prompts().len(),
        4,
        "各ターンの LLM 呼び出しが 1 回でない（沈黙ターンで余計な再生成が起きている）: {}",
        mock.system_prompts().len()
    );
}

