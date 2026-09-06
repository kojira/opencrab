use super::support::*;

// ==================== (a) message → say ＋ §9A e番号 ====================

#[tokio::test]
async fn scenario_a_message_becomes_say_and_conversation_has_e_number() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("700", &format!("{M_SAY} こんにちは、返事して"));

    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "say" && c.body.contains(B_SAY))
        })
        .await
    };
    assert!(
        ok,
        "message の say が dry-run に出ない: {:?}",
        captured(&buf)
    );

    // §5.4 typing: activity started で gateway が typing を打つ（dry-run kind="typing"）。ターンが
    // 走った（say が出た）＝ started/ended を観測しているので、typing keepalive が最低 1 回打つ。
    let saw_typing = {
        let buf = buf.clone();
        wait_until(move || count_kind(&buf, "typing") >= 1).await
    };
    assert!(
        saw_typing,
        "typing（§5.4）が dry-run に出ない: {:?}",
        captured(&buf)
    );

    // §9A: discord message が会話に e1 として現れる（core 汎化で discord kind に採番）。
    let saw_e1 = {
        let reqs = mock.request_texts();
        reqs.iter().any(|t| t.contains("e1") && t.contains(M_SAY))
    };
    assert!(
        saw_e1,
        "会話に e1（§9A e番号）が現れない（core 汎化が discord に採番していない）: {:?}",
        mock.request_texts()
    );
    // 生 snowflake（channel/message/author id）は会話へ出さない。
    for t in mock.request_texts() {
        assert!(
            !t.contains("discord:message:v1:"),
            "生 origin が会話に露出: {t}"
        );
    }
}

// ==================== (b) reply(e1, 本文) の実 DI 経路 ====================

#[tokio::test]
async fn scenario_b_reply_resolves_e_number_and_settles() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("701", &format!("{M_REPLY} これに返信して"));

    // LLM tool_call reply(e1) → core が e1→origin 解決 → invoke → gateway dry-run reply → 決着。
    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_REPLY))
        })
        .await
    };
    assert!(
        ok,
        "reply の実 DI 経路が決着しない（e1 解決 or invoke or settle 失敗）: {:?}",
        captured(&buf)
    );
    // 発端 701 に限定して数える（BUFFER は binary 全体で共有・累積するため、他テストの reply
    // 配送を数え込まないよう message で scope する。ハーネス棚卸し・相互汚染の是正）。
    let replies_for_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "reply" && c.message == "701")
        .count();
    assert_eq!(
        replies_for_origin, 1,
        "reply が複数回 or 0 回: 自動再送 0（発端 701 に限定）"
    );
    // e1 が発端メッセージ（channel=600, message=701）へ正しく解決されている（誤解決検知）。
    // BUFFER は共有・累積のため、本テストの reply（body=B_REPLY）に限定して取り出す。
    let reply = captured(&buf)
        .into_iter()
        .find(|c| c.kind == "reply" && c.body.contains(B_REPLY))
        .unwrap();
    assert_eq!(
        reply.channel, CHANNEL,
        "reply 対象 channel が発端と不一致（e1 誤解決）"
    );
    assert_eq!(
        reply.message, "701",
        "reply 対象 message が発端と不一致（e1 誤解決）"
    );
}

// ==================== (c) reaction(e1, emoji) の実 DI 経路 ====================

#[tokio::test]
async fn scenario_c_reaction_resolves_e_number_and_settles() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("702", &format!("{M_REACT} これにリアクションして"));

    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reaction" && c.emoji.contains(EMOJI))
        })
        .await
    };
    assert!(
        ok,
        "reaction の実 DI 経路が決着しない: {:?}",
        captured(&buf)
    );
    // 発端 702 に限定して数える（BUFFER は共有・累積のため、他テストの reaction 配送を
    // 数え込まないよう message で scope する。ハーネス棚卸し・相互汚染の是正）。
    let reactions_for_origin = captured(&buf)
        .iter()
        .filter(|c| c.kind == "reaction" && c.message == "702")
        .count();
    assert_eq!(
        reactions_for_origin, 1,
        "reaction が複数回 or 0 回: 自動再送 0（発端 702 に限定）"
    );
    // e1 が発端メッセージ（channel=600, message=702）へ正しく解決されている（誤解決検知）。
    let react = captured(&buf)
        .into_iter()
        .find(|c| c.kind == "reaction" && c.message == "702")
        .unwrap();
    assert_eq!(
        react.channel, CHANNEL,
        "reaction 対象 channel が発端と不一致（e1 誤解決）"
    );
    assert_eq!(
        react.message, "702",
        "reaction 対象 message が発端と不一致（e1 誤解決）"
    );
}

// ==================== (d) system reaction（👀 受理・🏁 完了）の V3 経路 ====================

#[tokio::test]
async fn scenario_d_system_reactions_accepted_and_completed() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    // M_SAY: turn は通常発言（say）を返す。agent の reaction DI は使わない
    // （＝ kind="reaction" は 0・system reaction は kind="system_reaction" で分離観測）。
    fixture.append_message("703", &format!("{M_SAY_D} 受理と完了のサインを見たい"));

    // 👀: LLM がこの発端メッセージをターン文脈に含めた（読んだ）時点＝activity started(origin) で
    // 発端メッセージ（channel=600, message=703）へ付く（R2・受理/推論前では付けない）。
    let saw_accepted = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf).iter().any(|c| {
                c.kind == "system_reaction"
                    && c.emoji.contains(SYS_ACCEPTED)
                    && c.channel == CHANNEL
                    && c.message == "703"
            })
        })
        .await
    };
    assert!(
        saw_accepted,
        "受理 👀（system_reaction）が発端メッセージに付かない: {:?}",
        captured(&buf)
    );

    // 🏁: DESIGN-TURN-CONTINUATION §13.2「activity ended を受けた時点で、そのターンで最後に
    // 成功した自分の say メッセージに 1 件だけ」。単発 say ターンなので最後の投稿 ＝ この 1 件の say。
    // §13.2 表 row 1（ターンが投稿で終わる → 最後の投稿に 1）。own message id で相関し、`any` では
    // なく**総数 == 1**で pin する（say ごとに 🏁 を付ける実装なら総数が増える → 検知）。
    let own_say_mids: Vec<String> = {
        let wbuf = buf.clone();
        // 最後の say（＝この単発 say）の own message id に 🏁 が付くまで待つ。
        wait_until(move || {
            let caps = captured(&wbuf);
            let mids: Vec<String> = caps
                .iter()
                .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(B_SAY_D))
                .map(|c| c.message.clone())
                .filter(|m| !m.is_empty())
                .collect();
            !mids.is_empty()
                && caps.iter().any(|c| {
                    c.kind == "system_reaction"
                        && c.emoji.contains(SYS_COMPLETED)
                        && c.channel == CHANNEL
                        && mids.contains(&c.message)
                })
        })
        .await;
        captured(&buf)
            .iter()
            .filter(|c| c.kind == "say" && c.channel == CHANNEL && c.body.contains(B_SAY_D))
            .map(|c| c.message.clone())
            .filter(|m| !m.is_empty())
            .collect()
    };
    assert_eq!(
        own_say_mids.len(),
        1,
        "単発 say の own message id が 1 件でない: {:?}",
        captured(&buf)
    );
    let completed_on_own = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction"
                && c.emoji.contains(SYS_COMPLETED)
                && c.channel == CHANNEL
                && own_say_mids.contains(&c.message)
        })
        .count();
    assert_eq!(
        completed_on_own,
        1,
        "完了 🏁 が自分の最後の say に 1 件で付かない（§13.2・ターン終了時のみ）: {:?}",
        captured(&buf)
    );

    // 付け先誤りの是正: 🏁 は発端メッセージ（703）には**付かない**。
    let completed_on_origin = captured(&buf).iter().any(|c| {
        c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "703"
    });
    assert!(
        !completed_on_origin,
        "🏁 が発端メッセージ 703 に誤って付いている（#869 の付け先取り違え）: {:?}",
        captured(&buf)
    );

    // agent の reaction DI（kind="reaction"）は 703 に対しては起きていない（system reaction を
    // agent reaction と混同していない）。BUFFER は binary 内で共有なので発端 703 に絞って観測する。
    let agent_reactions_on_703 = captured(&buf)
        .iter()
        .filter(|c| c.kind == "reaction" && c.message == "703")
        .count();
    assert_eq!(
        agent_reactions_on_703,
        0,
        "M_SAY ターンで agent reaction が 703 に誤発火: {:?}",
        captured(&buf)
    );

    // 受理 👀 は発端 1 メッセージにつき 1 回（自動再送 0）。
    let accepted_count = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_ACCEPTED) && c.message == "703"
        })
        .count();
    assert_eq!(accepted_count, 1, "受理 👀 が複数回: {:?}", captured(&buf));

    // 返信したターン（M_SAY）には 🤐 は付かない（裁定A: core が ended を say の後に出すので
    // 返信ターンで CompletedNoReply が立たない）。
    let noreply_on_703 = captured(&buf)
        .iter()
        .filter(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "703")
        .count();
    assert_eq!(
        noreply_on_703,
        0,
        "返信ターンに 🤐 が誤発火（core reorder が効いていない）: {:?}",
        captured(&buf)
    );
}
