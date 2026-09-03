use super::SubtaskSteerInbound;
use opencrab_core::LiveInboundSource;

fn insert_log(db: &opencrab_db::Db, session_id: &str, log_type: &str, content: &str) {
    let conn = db.lock().unwrap();
    let log = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: String::new(),
        session_id: session_id.to_string(),
        log_type: log_type.to_string(),
        content: content.to_string(),
        speaker_id: None,
        turn_number: None,
        metadata_json: None,
        created_at: None,
    };
    opencrab_db::queries::insert_session_log_best_effort(&conn, &log);
}

/// #647: steer ログだけを差分注入し、同じ steer を二度返さない（watermark）。通常発話や
/// system ログは拾わない。watermark 初期値は 0（セッション先頭）なので、source 構築の
/// 直前に届いた steer も取りこぼさない（「Accepted なのに読まれない」窓を閉じる）。
#[test]
fn polls_only_new_steer_logs_and_dedups() {
    let conn = opencrab_db::init_memory().unwrap();
    let db = opencrab_db::Db::from_connection(conn);
    let sub = "subtask-abc";

    // source 構築の**前**に届いた steer も、watermark=0 なので初回 poll で拾える
    // （spawn 直後〜engine 準備完了の窓での消失を防ぐ / レビュー指摘 1）。
    insert_log(&db, sub, opencrab_actions::STEER_LOG_TYPE, "早い steer");
    let src = SubtaskSteerInbound::new(db.clone(), sub);

    // 構築後にもう 1 本届く。
    insert_log(&db, sub, opencrab_actions::STEER_LOG_TYPE, "JSON で出して");
    // 別 log_type は拾わない。
    insert_log(&db, sub, "speech", "これは発話（対象外）");
    insert_log(&db, sub, "system", "これは system（対象外）");

    let first = src.poll_new_messages();
    assert_eq!(first.len(), 2, "構築前後の steer を両方拾う");
    assert!(
        first[0].contains("早い steer"),
        "構築前の steer も取りこぼさない"
    );
    assert!(first[1].contains("JSON で出して"), "本文が載る");
    assert!(first[0].contains("追加指示"), "steer と分かる整形が付く");

    // 2 回目は新着なし（watermark が進んでいる）。
    assert!(
        src.poll_new_messages().is_empty(),
        "同じ steer を二度注入しない"
    );

    // さらに届いた steer は次の poll で拾う。
    insert_log(&db, sub, opencrab_actions::STEER_LOG_TYPE, "2 通目");
    let third = src.poll_new_messages();
    assert_eq!(third.len(), 1);
    assert!(third[0].contains("2 通目"));
}
