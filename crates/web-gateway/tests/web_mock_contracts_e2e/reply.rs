
/// 裁定A の web 退行固定: 返信した（say を出す）ターンでは SSE に `event: message` が流れ、
/// `event: completed_no_reply` は**流れない**。旧実装は activity ended を say より先に出していたため
/// 返信ターンでも completed_no_reply を誤発火していた（core reorder で撤去）。
#[test]
fn reply_turn_does_not_emit_completed_no_reply() {
    const B_REPLY: &str = "replybody-omega-answer";
    // 何が来ても通常の返信本文（＝say）を返す。
    let mock = spawn_mock(|_req, _gate| text_resp(B_REPLY));
    let h = setup(mock.port, "replyturn", false, "[tools]\nenabled = false\n");
    let session = &h.session;

    let sse_rx = spawn_sse_collect(h.gw_port, session);
    std::thread::sleep(Duration::from_millis(300));
    post_message(
        h.gw_port,
        session,
        "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
        "返事して",
    );

    let sse = sse_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("sse buffer");
    assert!(
        sse.contains("event: message"),
        "返信ターンなのに SSE へ message(say) が流れない: {sse:?}"
    );
    assert!(
        !sse.contains("event: completed_no_reply"),
        "返信ターンで completed_no_reply が誤発火（裁定A の core reorder が効いていない）: {sse:?}"
    );
}
