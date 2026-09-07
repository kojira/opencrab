
// ==================== 契約 1: NO_REPLY / withheld ====================

/// mock が `content:"NO_REPLY"` を返すターンは、SSE に `event: message` として流れず
/// `event: completed_no_reply` として観測でき、say / delivery は起きない。
///
/// #899 §12.6: 沈黙の監査行 `speech='NO_REPLY'`（旧: `no_reply:true`）も **DB に残さない**。
/// withheld の可視化は SSE `completed_no_reply`（`LiveEvent::CompletedNoReply` 由来・DB 非依存）が担う。
#[test]
fn no_reply_is_withheld_not_said() {
    // 何が来ても沈黙（NO_REPLY）を返す。
    let mock = spawn_mock(|_req, _gate| text_resp("NO_REPLY"));
    let h = setup(mock.port, "noreply", false, "[tools]\nenabled = false\n");
    let session = &h.session;
    let binding = &h.binding;
    let db = &h.db;
    let ext_session = format!("extgate-{binding}");

    // SSE を張ってから投稿し、このターンのイベントを拾う。
    let sse_rx = spawn_sse_collect(h.gw_port, session);
    std::thread::sleep(Duration::from_millis(300));
    post_message(
        h.gw_port,
        session,
        "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        "沈黙して",
    );

    // 沈黙の決着シグナルは SSE completed_no_reply（DB 行に依存しない・http.rs の
    // LiveEvent::CompletedNoReply 由来）。これを決着バリアに使う。
    let sse = sse_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("sse buffer");
    assert!(
        sse.contains("event: completed_no_reply"),
        "SSE に completed_no_reply が来ない: {sse:?}"
    );
    assert!(
        !sse.contains("event: message"),
        "NO_REPLY なのに SSE へ say(message) が流れた: {sse:?}"
    );

    // #899 §12.6: NO_REPLY のみは speech を保存しない（旧実装は withheld 監査行
    // content='NO_REPLY' / no_reply:true を残していたが、typed 履歴へ assistant 'NO_REPLY' として
    // 再注入されるため撤去）。沈黙の可視化は上の SSE completed_no_reply が担う。
    let no_reply_rows: i64 = {
        let conn = open_db(db);
        conn.query_row(
            "SELECT COUNT(*) FROM memory_sessions
             WHERE session_id = ?1 AND content = 'NO_REPLY' AND speaker_id = ?2",
            rusqlite::params![ext_session, AGENT],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    assert_eq!(
        no_reply_rows, 0,
        "NO_REPLY のみなのに speech='NO_REPLY' が保存された（#899・§12.6）: {no_reply_rows}"
    );

    // say（external_response）は記録されていない。
    let says: i64 = {
        let conn = open_db(db);
        conn.query_row(
            "SELECT COUNT(*) FROM memory_sessions
             WHERE session_id = ?1 AND metadata_json LIKE '%external_response%'",
            rusqlite::params![ext_session],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    assert_eq!(says, 0, "NO_REPLY なのに say が記録された");

    // deliveries（say の配送）も無い。
    let delivered: i64 = {
        let conn = open_db(db);
        conn.query_row("SELECT COUNT(*) FROM deliveries", [], |r| r.get(0))
            .unwrap_or(0)
    };
    assert_eq!(delivered, 0, "NO_REPLY なのに delivery が作られた");
}
