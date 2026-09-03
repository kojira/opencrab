use super::{build_conversation_string, RECENT_MIN_USER_SPEECHES};
use opencrab_actions::transcript::{InboundMessageRecord, TranscriptSource};

const AGENT: &str = "a1";
const USER: &str = "owner";
const SESSION: &str = "s1";

/// ユーザー発言を**本番と同じ書き手**（`record_inbound_message`）で入れる。
/// 行の形（`agent_id` 列＝受信側エージェント / `speaker_id` 列＝送信者、#377）を
/// 再現するのがこのテストの肝。
fn insert_user_speech(conn: &rusqlite::Connection, text: &str) {
    assert!(
        crate::transcript::record_inbound_message(
            conn,
            TranscriptSource::Discord,
            &InboundMessageRecord {
                session_id: SESSION,
                recipient_agent_id: AGENT,
                sender_id: USER,
                sender_name: "owner",
                avatar_url: None,
                channel_id: Some("222"),
                pubkey: None,
                text,
                image_urls: &[],
            },
        ),
        "テストの前提: 受信発言が記録できること"
    );
}

/// エージェント自身の行（発言 / ツール往復）。こちらは `agent_id == speaker_id == AGENT`。
fn insert_agent_row(conn: &rusqlite::Connection, log_type: &str, content: &str) -> i64 {
    opencrab_db::queries::insert_session_log(
        conn,
        &opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: AGENT.to_string(),
            session_id: SESSION.to_string(),
            log_type: log_type.to_string(),
            content: content.to_string(),
            speaker_id: Some(AGENT.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        },
    )
    .unwrap()
}

fn last_log_id(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT MAX(id) FROM memory_sessions", [], |r| r.get(0))
        .unwrap()
}

/// ユーザー発言のあとに巨大なツール結果が大量に積まれても、発言は残る。
#[test]
fn user_speech_survives_a_flood_of_tool_results() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_user_speech(&conn, "もういっそ全員フォローすればいいよ");
    // 直近 10 件を tool_result / 自分の発言で埋める（当時と同じ形）。
    for i in 0..12 {
        insert_agent_row(
            &conn,
            "tool_result",
            &format!("Following 979 user(s): {}", "npub1xxxx ".repeat(200 + i)),
        );
    }
    for i in 0..3 {
        insert_agent_row(&conn, "speech", &format!("確認中です（{i}）"));
    }

    // 全文が入らない予算（＝コンパクション経路）。
    let out = build_conversation_string(&conn, SESSION, AGENT, 500).unwrap();
    assert!(
        out.contains("もういっそ全員フォローすればいいよ"),
        "直近のユーザー発言がプロンプトから落ちている: {out}"
    );
}

/// ユーザー発言が要約境界より前に落ちていても混ぜ戻される。
#[test]
fn user_speech_is_reinjected_from_before_the_summary_boundary() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_user_speech(&conn, "つらい");
    let user_log = last_log_id(&conn);
    for _ in 0..20 {
        insert_agent_row(&conn, "tool_result", &"x".repeat(400));
    }
    // 現セッションの topic 要約が user_log を含む範囲をカバーしている状態を作る。
    opencrab_db::queries::insert_index_node(
        &conn,
        &opencrab_db::queries::IndexNodeRow {
            id: "t1".to_string(),
            agent_id: AGENT.to_string(),
            parent_id: None,
            node_type: "topic".to_string(),
            source_type: "session_log".to_string(),
            title: "過去の話題".to_string(),
            summary: "過去の要約".to_string(),
            start_log_id: Some(1),
            end_log_id: Some(user_log + 10),
            source_session_id: Some(SESSION.to_string()),
            date_from: None,
            date_to: None,
            depth: 0,
            child_count: 0,
            token_count: 0,
            created_at: "2026-07-31T00:00:00Z".to_string(),
            updated_at: "2026-07-31T00:00:00Z".to_string(),
            short_id: Some("t1".to_string()),
            keywords_json: "[]".to_string(),
            summary_refreshed_at: None,
        },
    )
    .unwrap();

    let out = build_conversation_string(&conn, SESSION, AGENT, 400).unwrap();
    assert!(
        out.contains("つらい"),
        "要約境界より前のユーザー発言が混ぜ戻されていない: {out}"
    );
}

/// 保証するのは**直近 N 件**であって全件ではない（古い発言まで無条件に積むと
/// 予算保証が壊れる）。
#[test]
fn only_the_most_recent_user_speeches_are_forced_in() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_user_speech(&conn, "とても古い発言マーカー");
    for i in 0..RECENT_MIN_USER_SPEECHES {
        insert_user_speech(&conn, &format!("新しい発言 {i}"));
    }
    for _ in 0..15 {
        insert_agent_row(&conn, "tool_result", &"y".repeat(400));
    }
    let out = build_conversation_string(&conn, SESSION, AGENT, 300).unwrap();
    assert!(
        !out.contains("とても古い発言マーカー"),
        "N 件を超えて古い発言まで強制的に載せている: {out}"
    );
    assert!(out.contains(&format!("新しい発言 {}", RECENT_MIN_USER_SPEECHES - 1)));
}

/// 予算に余裕があるときは従来どおり全文が出る（回帰防止）。
#[test]
fn full_conversation_is_unchanged_when_it_fits() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_user_speech(&conn, "こんにちは");
    insert_agent_row(&conn, "speech", "はい");
    let out = build_conversation_string(&conn, SESSION, AGENT, 100_000).unwrap();
    assert!(out.contains("こんにちは"));
    assert!(out.contains("はい"));
    assert!(!out.contains("omitted"));
}

/// **Discord/Nostr の行の形**（`record_inbound_message` 経由）でも直近ユーザー発言が
/// 優先枠に入ること。fixture は本番と同じ書き手を使い、受信行が `agent_id`＝受信側 /
/// `speaker_id`＝送信者（#377）で入ることも下で固定する。
///
/// **このテストはもう #286 を pin していない**（識別力が下がった点は正直に書く）:
/// #286 は「受信行が `agent_id == speaker_id` になり、列比較の述語
/// `speaker_id != log.agent_id` が恒偽になる」バグだった。#377 で受信行が
/// `agent_id != speaker_id`（受信側≠送信者）に直ったため、仮に述語を旧・列比較へ
/// 戻しても "owner" != "a1" で真になり、**このテストは落ちない**。
///
/// それでも無防備になった性質は無い: 行の形が直ったので列比較でも正しい答えになる
/// （＝ #286 のバグ自体が成立しなくなった）。述語が引数比較であるべきことは
/// `opencrab_core::conversation::is_user_speech` の doc とその近傍テストが担い、ここは「ゲートウェイ形状の
/// 発言が必須枠に載る」という結果だけを固定する。
#[test]
fn gateway_shaped_rows_are_recognized_as_user_speech() {
    let conn = opencrab_db::init_memory().unwrap();
    insert_user_speech(&conn, "この発言が消えたら対話が成立しない");
    // 予算を食い潰す巨大なツール往復（末尾の連続区間を占有する）。
    for _ in 0..30 {
        insert_agent_row(&conn, "tool_result", &"z".repeat(600));
    }
    // 受信行が本番と同じ形（agent_id 列＝受信側エージェント / speaker_id 列＝送信者、
    // #377）で入っていることを固定する。
    let (row_agent, row_speaker): (String, String) = conn
        .query_row(
            "SELECT agent_id, speaker_id FROM memory_sessions \
                 WHERE log_type = 'speech' AND speaker_id = ?1 LIMIT 1",
            [USER],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (row_agent.as_str(), row_speaker.as_str()),
        (AGENT, USER),
        "受信行は agent_id 列＝受信側エージェント / speaker_id 列＝送信者（#377）"
    );

    let out = build_conversation_string(&conn, SESSION, AGENT, 300).unwrap();
    assert!(
        out.contains("この発言が消えたら対話が成立しない"),
        "ゲートウェイ形状のユーザー発言が優先枠に入っていない: {out}"
    );
}
