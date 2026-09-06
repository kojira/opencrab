use super::support::*;

// ==================== (e) system reaction 🤐（NO_REPLY）の V3 経路 ====================

#[tokio::test]
async fn scenario_e_no_reply_gets_muted_reaction() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    // M_NOREPLY: turn は NO_REPLY（say 無し）。受理 👀 のあと、沈黙決着で 🤐 が発端へ付く。
    fixture.append_message("704", &format!("{M_NOREPLY} これは黙って"));

    // 🤐: 沈黙ターンの決着（CompletedNoReply・reply_origin=Single）で発端 704 へ付く。
    let saw_muted = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf).iter().any(|c| {
                c.kind == "system_reaction"
                    && c.emoji.contains("🤐")
                    && c.channel == CHANNEL
                    && c.message == "704"
            })
        })
        .await
    };
    assert!(
        saw_muted,
        "NO_REPLY 🤐（system_reaction）が発端メッセージに付かない: {:?}",
        captured(&buf)
    );

    // §13.2 表 row 11/13（NO_REPLY のみ → 🏁 0・沈黙終了は 🤐）。沈黙ターンは自分の投稿が無い
    // ので 🏁 は付かない。発端 704 への誤付与を総数 0 で pin する（この turn の自投稿 id は無い）。
    let completed_on_704 = captured(&buf)
        .iter()
        .filter(|c| {
            c.kind == "system_reaction" && c.emoji.contains(SYS_COMPLETED) && c.message == "704"
        })
        .count();
    assert_eq!(
        completed_on_704,
        0,
        "NO_REPLY ターンに 🏁 が誤発火（§13.2 row 11/13）: {:?}",
        captured(&buf)
    );

    // #899 / §12.6: 沈黙決着で 🤐 は付くが、speech='NO_REPLY' の監査行は残さない。
    // （裸 NO_REPLY を永続すると conversation_typed が assistant 'NO_REPLY' として再注入する。）
    let no_reply_rows: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}' AND content='NO_REPLY'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        no_reply_rows, 0,
        "NO_REPLY のみのターンで speech='NO_REPLY' が保存された（#899・§12.6）: {no_reply_rows}"
    );
}

// ==================== (g) reply×N＋NO_REPLY（§13 #14）: reply 保存・NO_REPLY 行なし・🤐なし ====================

/// §13 #14: 発話 op（reply）を出したターンの末尾が NO_REPLY でも、reply は配送/保存され、
/// 末尾 NO_REPLY は speech 行を足さない（#899）。発話があるので 🤐 は付かない。
///
/// | 観測点 | 期待 |
/// |---|---|
/// | reply 配送（dry-run kind=reply） | 1（本文 B_REPLY_NR） |
/// | speech='NO_REPLY' 保存 | 0 |
/// | 🤐（system_reaction）on 発端 | 0（発話ありターン） |
///
/// 現 tip で赤: 末尾 NO_REPLY が record_agent_no_reply で `content='NO_REPLY'` を保存する。
#[tokio::test]
async fn scenario_g_reply_then_no_reply_saves_reply_not_no_reply() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    // 705: 同一生成で reply op ＋ content=NO_REPLY（#904 の M_REPLY_NR 契約）。
    fixture.append_message("705", &format!("{M_REPLY_NR} 返信してから黙って"));

    // reply（一意本文）が配送されるまで待つ。on_tool_call の content 保存は reply 実行の
    // **前**に走るので、この時点で（現 tip なら）NO_REPLY 行は既に書かれている。
    let ok = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_REPLY_NR))
        })
        .await
    };
    assert!(ok, "reply(705) が配送されない: {:?}", captured(&buf));

    // reply は 1 回だけ（一意本文なので他テスト混線なし）。
    let reply_count = captured(&buf)
        .iter()
        .filter(|c| c.kind == "reply" && c.body.contains(B_REPLY_NR))
        .count();
    assert_eq!(
        reply_count,
        1,
        "reply が 1 回配送されていない（§13 #14）: {:?}",
        captured(&buf)
    );

    // #899: reply があっても末尾 / 同一生成の NO_REPLY は speech='NO_REPLY' 行を足さない
    //（on_tool_call の content 保存経路・配送層 NoReply の両方）。
    let no_reply_rows: i64 = {
        let conn = core.extgate.db.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM memory_sessions WHERE log_type='speech' \
                 AND speaker_id='{AGENT_ID}' AND content='NO_REPLY'"
            ),
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        no_reply_rows, 0,
        "reply×N＋NO_REPLY のターンで speech='NO_REPLY' が保存された（#899・§13 #14）: {no_reply_rows}"
    );
    // 注: 「発話ありターンに 🤐 を付けない」は既存 scenario_d が担保（本テストは #899 の保存側に集中）。
}

// ==================== (e2) reply（発話 invoke）ターンには 🤐 が付かない（#900） ====================
//
// #900: reply/reaction は say ではなく invoke で配送される。gate は「発話（say/reply/reaction）が
// 1 つでもあれば沈黙ではない」と判定すべきで、reply しかしていないターンを最終本文空＝沈黙と解釈して
// 🤐 を付けてはならない。reply 配送後、ターン決着（activity ended）で 🤐 が付くならその時点で出るので、
// 🤐 の出現を bounded poll で待って「出ない」ことを確定する（バグ時は即座に 🤐 が出て RED）。
#[tokio::test]
async fn scenario_e2_reply_turn_does_not_get_muted_reaction() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;

    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    // 701: reply ターン（発話は invoke 経路・say 無し）。
    fixture.append_message("701", &format!("{M_REPLY} これに返信して"));
    let replied = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_REPLY) && c.message == "701")
        })
        .await
    };
    assert!(replied, "reply が 701 へ配送されない: {:?}", captured(&buf));

    // 701（reply ターン）に 🤐 が付かない。バグ時は決着で 🤐 が即座に出るので wait_until が true に
    // なって RED。修正後は発話ありと判定されて 🤐 が出ず、poll は timeout して false（GREEN）。
    let muted_on_701 = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf).iter().any(|c| {
                c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == "701"
            })
        })
        .await
    };
    assert!(
        !muted_on_701,
        "reply ターン(701)に 🤐 が誤発火（発話を沈黙扱いした）: {:?}",
        captured(&buf)
    );
}

/// 発端 origin に 🤐（system_reaction）が付いていないことを bounded poll で確定する共通ヘルパー。
/// バグ時は決着で 🤐 が即座に出て poll が true→assert 失敗（RED）。修正後は 🤐 が出ず false（GREEN）。
async fn assert_no_muted_on(buf: &Arc<Mutex<Vec<Captured>>>, message: &str) {
    let b = buf.clone();
    let m = message.to_string();
    let muted = wait_until(move || {
        captured(&b)
            .iter()
            .any(|c| c.kind == "system_reaction" && c.emoji.contains("🤐") && c.message == m)
    })
    .await;
    assert!(
        !muted,
        "発話ありターン({message})に 🤐 が誤発火（発話を沈黙扱いした）: {:?}",
        captured(buf)
    );
}

// ==================== (e3) §13 #6: reply×3 in one ターンには 🤐 が付かない（#900） ====================
#[tokio::test]
async fn scenario_e3_reply3_in_one_turn_does_not_get_muted_reaction() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("706", &format!("{M_REPLY3} 3回に分けて返信して"));
    let all_replied = {
        let buf = buf.clone();
        wait_until(move || {
            B_REPLY3.iter().all(|b| {
                captured(&buf)
                    .iter()
                    .any(|c| c.kind == "reply" && c.body.contains(b) && c.message == "706")
            })
        })
        .await
    };
    assert!(
        all_replied,
        "reply×3 in one が全配送されない: {:?}",
        captured(&buf)
    );
    // 発話（reply）が 3 つあったので沈黙ではない → 🤐 は付かない（§13 #6）。
    assert_no_muted_on(&buf, "706").await;
}

// ==================== (e4) §13 #9: reply＋末尾 CONTINUE ターンには 🤐 が付かない（#900） ====================
#[tokio::test]
async fn scenario_e4_reply_then_continue_turn_does_not_get_muted_reaction() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("707", &format!("{M_REPLY_CONT} 返信して続けて"));
    // 継続後の最終 reply も同じ 707 へ配送される（継続が起きた証拠）。少なくとも reply が届く。
    let replied = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_REPLY_CONT) && c.message == "707")
        })
        .await
    };
    assert!(
        replied,
        "reply＋CONTINUE ターンの reply が配送されない: {:?}",
        captured(&buf)
    );
    // 発話（reply）があったので 🤐 は付かない（§13 #9）。
    assert_no_muted_on(&buf, "707").await;
}

// ==================== (e5) §13 #14: reply＋NO_REPLY ターンには 🤐 が付かない（#900） ====================
#[tokio::test]
async fn scenario_e5_reply_then_no_reply_turn_does_not_get_muted_reaction() {
    let buf = install_capture();
    let mock = Arc::new(RoutedMock::new());
    let core = start_core(mock.clone() as Arc<dyn LlmProvider>).await;
    let fixture = Fixture::new();
    let _client = wire_instance(&core, &fixture).await;

    fixture.append_message("708", &format!("{M_REPLY_NR} 返信して黙って"));
    let replied = {
        let buf = buf.clone();
        wait_until(move || {
            captured(&buf)
                .iter()
                .any(|c| c.kind == "reply" && c.body.contains(B_REPLY_NR) && c.message == "708")
        })
        .await
    };
    assert!(
        replied,
        "reply＋NO_REPLY ターンの reply が配送されない: {:?}",
        captured(&buf)
    );
    // 最終本文は NO_REPLY だが、そのターンに reply（発話）があるので 🤐 は付かない（§13 #14）。
    assert_no_muted_on(&buf, "708").await;
}
