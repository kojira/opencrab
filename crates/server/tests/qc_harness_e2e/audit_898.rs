// ---------------------------------------------------------------------------
// #898【DESIGN-TURN-CONTINUATION §13 #2（本文＋最終行 CONTINUE→進む）を 3 連鎖／ターン合計 plain3・
// §13.1 b/c/d（1 イテレーション=1 投稿・順序保持）】: reply なし・CONTINUE で 3 分割 → 配送 3・保存 3・LLM 3・残留なし。
//
// 現 tip: 機構（CONTINUE 3 イテレーション）は動くが配送/保存は最終応答（er.response）だけを
// 通すので「3回目」だけが say/speech に残る（配送 1・保存 1）。→ 赤。
// 期待（設計 §11.1）: 途中イテレーションの発話も 1 回ずつ配送・保存される。
// ---------------------------------------------------------------------------
const AUD898_1: &str = "AUD898-ONE 監査一回目";
const AUD898_2: &str = "AUD898-TWO 監査二回目";
const AUD898_3: &str = "AUD898-THREE 監査三回目";

#[tokio::test]
async fn audit_898_continue_split_delivers_and_saves_each_iteration() {
    let buf = install_capture();
    let mock = Arc::new(FifoMock::new());
    // 各生成の末尾に単独行 CONTINUE（最終だけ無し）。engine は剥がして次イテレーションへ。
    mock.push_text(&format!("{AUD898_1}\nCONTINUE"));
    mock.push_text(&format!("{AUD898_2}\nCONTINUE"));
    mock.push_text(AUD898_3);
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let (_client, _address, session_id) = wire_instance(&core, &fixture, nostr_config(None)).await;

    let ev = "8981".repeat(16);
    fixture.append_line(&mention_event(
        &ev,
        "C898-MARK 3回に分けて投稿して reply使わずに",
    ));

    // 最終イテレーションの配送が出るまで待つ（= 3 イテレーションまで到達した）。
    let done = {
        let buf = buf.clone();
        wait_until(move || body_index(&buf, AUD898_3).is_some()).await
    };
    assert!(
        done,
        "3回目の配送が出ない（CONTINUE 機構未到達）: {:?}",
        captured(&buf)
    );

    // 途中イテレーションも配送されるだけの猶予（現 tip では出ないので落ち着き待ち）。
    tokio::time::sleep(Duration::from_millis(400)).await;

    // 配送: 3 本すべてが standalone say として出る（現 tip は AUD898_3 のみ → 赤）。
    for body in [AUD898_1, AUD898_2, AUD898_3] {
        let n = captured(&buf)
            .iter()
            .filter(|c| c.kind == "standalone" && c.body.contains(body))
            .count();
        assert_eq!(
            n,
            1,
            "途中発話 {body} の配送回数が 1 でない（#898: 途中イテレーションが配送されない）: {:?}",
            captured(&buf)
        );
    }

    // 保存: speech に 3 本すべてが残る（現 tip は AUD898_3 のみ → 赤）。
    let saved = agent_speech_contents(&core, &session_id);
    for body in [AUD898_1, AUD898_2, AUD898_3] {
        assert!(
            saved.iter().any(|s| s.contains(body)),
            "途中発話 {body} が speech に保存されていない（#898）: {saved:?}"
        );
    }

    // LLM 回数: 3 生成（機構は動くので base でも 3・回帰の下限固定）。
    assert_eq!(
        mock.system_prompts().len(),
        3,
        "CONTINUE 3 分割の LLM 呼び出しが 3 でない"
    );

    // 残留マーカー: この分割ターンの say / speech に CONTINUE が残らない（§11.6）。
    // BUFFER は binary 全体で共有・累積するため、C898 マーカーを含む say に限定して判定する。
    assert!(
        captured(&buf)
            .iter()
            .filter(|c| [AUD898_1, AUD898_2, AUD898_3]
                .iter()
                .any(|m| c.body.contains(m)))
            .all(|c| !c.body.contains("CONTINUE")),
        "say body に CONTINUE が残留: {:?}",
        captured(&buf)
    );
    assert!(
        saved.iter().all(|s| !s.contains("CONTINUE")),
        "speech content に CONTINUE が残留: {saved:?}"
    );
}

