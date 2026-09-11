
// ==================== 契約 2: 非ブロッキング（長処理中の第2依頼） ====================

// 会話へ現れるルーティング用マーカー（互いに部分文字列にならない）。
const M_FIRST: &str = "MARKERONE-longop";
const M_SECOND: &str = "MARKERTWO-question";
const M_SUBTASK: &str = "MARKERSUB-worktask";
// say として観測する応答本文（マーカーとも互いとも部分一致しない ASCII）。
const B_ACK: &str = "ackbody-alpha-started";
const B_SECOND: &str = "secondbody-beta-answer";
const B_COMPLETION: &str = "completionbody-gamma-done";
const B_SUBTASK_RESULT: &str = "subresult-delta-internal";

/// qc_harness_e2e の scenario_main の web 版。長処理（保持中の背景サブタスク）走行中に投じた
/// 第2依頼が待たされず即応し、3 say が 1→2→3 の順で残る。
#[test]
fn second_request_not_blocked_during_long_op() {
    // ルーティングは qc の RoutedMock と同順（E→B→D→A→C）。
    let mock = spawn_mock(|req, gate| {
        // (E) subtask 決着後の resume → 完了報告 say(3)。B_SUBTASK_RESULT を最優先で見る。
        if req.contains(B_SUBTASK_RESULT) {
            return finished_text_resp(B_COMPLETION);
        }
        // (B) 親ターン#1 の spawn_subtask 実行後（tool 結果あり）→ 即応 ack say(1)。
        if has_tool_result(req) {
            return finished_text_resp(B_ACK);
        }
        // (D) 第2依頼 → 即応 say(2)。
        if req.contains(M_SECOND) {
            return finished_text_resp(B_SECOND);
        }
        // (A) 親ターン#1 初回 → spawn_subtask で背景サブタスクを detach。
        if req.contains(M_FIRST) {
            return tool_call_resp(
                "spawn_subtask",
                serde_json::json!({
                    "task": format!("{M_SUBTASK} 長い処理を実行して"),
                    "timeout_secs": 120,
                }),
            );
        }
        // (C) 背景サブタスク sub-run → release まで保持（＝長処理の走行中）。
        if req.contains(M_SUBTASK) {
            wait_release(gate);
            return finished_text_resp(B_SUBTASK_RESULT);
        }
        // その他（create の hello 等）は沈黙。
        text_resp("NO_REPLY")
    });
    let h = setup(
        mock.port,
        "nonblock",
        true, // auto_dispatch 有効（scenario_main と同じ）
        "[tools]\nenabled = false\n",
    );
    let db = &h.db;
    let ext_session = format!("extgate-{}", h.binding);

    // 1) 長処理依頼 → ack say(1) ＋ 背景サブタスク detach。
    post_message(
        h.gw_port,
        &h.session,
        "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        &format!("{M_FIRST} 長い処理して終わったら教えて"),
    );
    let ack_ready = wait_until(Duration::from_secs(30), || {
        first_log_id(db, &ext_session, B_ACK).is_some()
    });
    assert!(ack_ready, "ack say(1) が出ない");
    // この時点でサブタスクは走行中（未 release）＝完了報告はまだ無い。
    assert!(
        first_log_id(db, &ext_session, B_COMPLETION).is_none(),
        "release 前に完了報告が出ている（hold が効いていない）"
    );

    // 2) 走行中に第2依頼 → ブロックされず即応 say(2)。
    post_message(
        h.gw_port,
        &h.session,
        "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        &format!("{M_SECOND} 2 足す 2 は?"),
    );
    let second_ready = wait_until(Duration::from_secs(30), || {
        first_log_id(db, &ext_session, B_SECOND).is_some()
    });
    assert!(
        second_ready,
        "第2依頼が長処理にブロックされている（say(2) 未達）"
    );
    // 第2依頼が返った時点でもサブタスクはまだ走行中（未 release）＝完了報告は無い。
    assert!(
        first_log_id(db, &ext_session, B_COMPLETION).is_none(),
        "第2依頼処理中にサブタスクが既に完了している（非ブロック検証が無効）"
    );

    // 3) サブタスクを解放 → 決着 → resume → 完了報告 say(3)。
    mock.release();
    let completion_ready = wait_until(Duration::from_secs(30), || {
        first_log_id(db, &ext_session, B_COMPLETION).is_some()
    });
    assert!(completion_ready, "完了報告 say(3) が出ない");

    // 3 say が 1→2→3 の順で並ぶ。
    let i1 = first_log_id(db, &ext_session, B_ACK).expect("ack id");
    let i2 = first_log_id(db, &ext_session, B_SECOND).expect("second id");
    let i3 = first_log_id(db, &ext_session, B_COMPLETION).expect("completion id");
    assert!(
        i1 < i2 && i2 < i3,
        "say の順序が 1→2→3 でない: ack={i1} second={i2} completion={i3}"
    );
}
