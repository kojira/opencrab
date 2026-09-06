use super::super::*;
/// v32: 既存の受信行（送信者名義）を受信側エージェント名義へ付け替え、索引/FTS に
/// 載せる（#380）。session_id に埋まった agent_id で受信側を復元できる行だけを対象にし、
/// 復元できない行・新形・旧々形・応答は 1 行も触らないこと、FTS も同時に直ること、
/// FTS 検索へ受信側名義で載ること、冪等であることを固定する。索引ビルドについては
/// 「watermark を巻き戻せば載る形」かつ「watermark 先行下では載らない」の両側を固定する
/// （v32 が回復するのは FTS 検索のみで、索引への取り込みは #380 に残る）。
#[test]
fn v32_remaps_inbound_agent_id_to_recipient_and_indexes() {
    use crate::queries::{
        get_unindexed_session_logs, insert_session_log, search_session_logs, SessionLogRow,
    };
    let conn = crate::init_memory().expect("init");

    // 受信側エージェント（session_id に UUID が埋まる）。migration は agents と JOIN する。
    let recipient = "aaaaaaaa-1111-2222-3333-444444444444";
    conn.execute(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES (?1, 'r', 'p')",
        [recipient],
    )
    .unwrap();

    let mk = |agent: &str, session: &str, speaker: &str, content: &str, meta: Option<&str>| {
        SessionLogRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: session.to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some(speaker.to_string()),
            turn_number: None,
            metadata_json: meta.map(|m| m.to_string()),
            created_at: None,
        }
    };

    // (1) 旧形の受信行・discord（復元可能）: agent=speaker=送信者、session に recipient が埋まる。
    let id_discord = insert_session_log(
        &conn,
        &mk(
            "sender-d",
            &format!("discord-{recipient}-100-200"),
            "sender-d",
            "discord inbound apple",
            Some(r#"{"source":"discord"}"#),
        ),
    )
    .unwrap();
    // (2) 旧形の受信行・nostr（復元可能, pubkey 付き session）。
    let id_nostr = insert_session_log(
        &conn,
        &mk(
            "sender-n",
            &format!("nostr-{recipient}-deadbeef"),
            "sender-n",
            "nostr inbound banana",
            Some(r#"{"source":"nostr"}"#),
        ),
    )
    .unwrap();
    // (3) 旧形の受信行・nostr（復元可能, recipient 単独 session）。
    let id_nostr2 = insert_session_log(
        &conn,
        &mk(
            "sender-n2",
            &format!("nostr-{recipient}"),
            "sender-n2",
            "nostr inbound cherry",
            Some(r#"{"source":"nostr"}"#),
        ),
    )
    .unwrap();
    // (4) 復元不能: discord-{guild}-{channel}（agent_id が埋まっていない）→ 触らない。
    let id_unresolved = insert_session_log(
        &conn,
        &mk(
            "sender-u",
            "discord-100-200",
            "sender-u",
            "unresolved inbound durian",
            Some(r#"{"source":"discord"}"#),
        ),
    )
    .unwrap();
    // (5) 新形の受信行（既に受信側名義, agent≠speaker）→ 触らない。
    let id_newform = insert_session_log(
        &conn,
        &mk(
            recipient,
            &format!("discord-{recipient}-100-200"),
            "sender-x",
            "newform inbound elder",
            Some(r#"{"source":"discord"}"#),
        ),
    )
    .unwrap();
    // (6) 旧々形（metadata 無し・既に受信側名義）→ 触らない。
    let id_oldold = insert_session_log(
        &conn,
        &mk(
            recipient,
            &format!("discord-{recipient}-100-200"),
            "sender-y",
            "oldold inbound fig",
            None,
        ),
    )
    .unwrap();
    // (7) 応答行（source discord_response, agent=speaker=recipient）→ 触らない。
    let id_reply = insert_session_log(
        &conn,
        &mk(
            recipient,
            &format!("discord-{recipient}-100-200"),
            recipient,
            "reply grape",
            Some(r#"{"source":"discord_response"}"#),
        ),
    )
    .unwrap();

    // v31 起点へ落として run_migrations で v32 を実際に走らせる。
    conn.execute_batch("PRAGMA user_version = 31").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v32");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    let agent_of = |id: i64| -> String {
        conn.query_row(
            "SELECT agent_id FROM memory_sessions WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    let fts_agent_of = |id: i64| -> String {
        conn.query_row(
            "SELECT agent_id FROM memory_sessions_fts WHERE rowid=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    let speaker_of = |id: i64| -> String {
        conn.query_row(
            "SELECT speaker_id FROM memory_sessions WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };

    // (1)(2)(3) 受信側名義へ付け替わり、FTS も追随、speaker_id は送信者のまま。
    for (id, sender) in [
        (id_discord, "sender-d"),
        (id_nostr, "sender-n"),
        (id_nostr2, "sender-n2"),
    ] {
        assert_eq!(agent_of(id), recipient, "本体 agent_id が受信側へ");
        assert_eq!(fts_agent_of(id), recipient, "FTS agent_id が受信側へ");
        assert_eq!(speaker_of(id), sender, "speaker_id は送信者のまま");
    }

    // (4)(5)(6)(7) 触っていない。
    assert_eq!(agent_of(id_unresolved), "sender-u", "復元不能行は不変");
    assert_eq!(
        fts_agent_of(id_unresolved),
        "sender-u",
        "復元不能行の FTS も不変"
    );
    assert_eq!(agent_of(id_newform), recipient, "新形は不変");
    assert_eq!(agent_of(id_oldold), recipient, "旧々形は不変");
    assert_eq!(agent_of(id_reply), recipient, "応答行は不変");

    // 索引ビルド入力に受信側名義で載る（送信者名義では載らない）。
    //
    // ここは `after_id = 0`（＝索引 watermark を巻き戻した状態）での確認であって、
    // 「watermark を巻き戻せば受信側名義で載る形になっている」ことだけを固定している。
    // 実際の索引ビルドは watermark（`last_indexed_log_id`）を `after_id` へ渡す
    // （`crates/core/src/memory_index/index_builder.rs`）ため、**本番のように watermark が
    // 対象行より先行している状況では、v32 だけでは索引へ入らない**（直下でその側も固定する）。
    // 索引ビルドへの実取り込みには別途 #380 の対応が要る。
    let indexed = get_unindexed_session_logs(&conn, recipient, 0, 100).unwrap();
    assert!(indexed.iter().any(|r| r.content == "discord inbound apple"));
    assert!(indexed.iter().any(|r| r.content == "nostr inbound banana"));
    assert!(
        get_unindexed_session_logs(&conn, "sender-d", 0, 100)
            .unwrap()
            .is_empty(),
        "受信行が送信者名義で索引入力に残っている"
    );

    // watermark が対象行より先行している状態（本番がこれ）では、付け替えても索引ビルド
    // 入力には載らない。v32 の効き目が FTS 検索に限られることを明示的に固定する。
    //
    // watermark は**付け替え対象の最大 id**（=(3)）にする。全体の MAX(id) にすると結果が
    // 必ず空になり、付け替えが 1 行も起きていなくても通る空回りのテストになる。(3) を境に
    // すれば recipient 名義の (5)(6)(7) は結果に残るので、「クエリが何も返していないだけ」
    // ではなく「付け替え行**だけ**が watermark に切られている」ことを固定できる。
    let above_watermark = get_unindexed_session_logs(&conn, recipient, id_nostr2, 100).unwrap();
    assert!(
        above_watermark
            .iter()
            .any(|r| r.content == "newform inbound elder"),
        "watermark より上の受信側名義行は載る（フィルタが空を返しているだけではない証拠）"
    );
    assert!(
        !above_watermark
            .iter()
            .any(|r| r.content == "discord inbound apple"
                || r.content == "nostr inbound banana"
                || r.content == "nostr inbound cherry"),
        "watermark 先行下では付け替え行は索引ビルド入力に載らない（#380 の残課題）"
    );

    // FTS 記憶検索で受信側が相手の発言を引ける。送信者名義では引けない。
    let hits = search_session_logs(&conn, recipient, "apple", 10).unwrap();
    assert!(hits.iter().any(|h| h.content == "discord inbound apple"));
    assert!(search_session_logs(&conn, "sender-d", "apple", 10)
        .unwrap()
        .is_empty());
    // 復元不能行は送信者名義のまま（付け替えていない証拠 = 誤って混ぜていない）。
    assert!(search_session_logs(&conn, "sender-u", "durian", 10)
        .unwrap()
        .iter()
        .any(|h| h.content == "unresolved inbound durian"));

    // 冪等: 版を 31 へ戻して up() を再実行しても、付け替えは二重に起きない。
    let same_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE agent_id = speaker_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute_batch("PRAGMA user_version = 31").unwrap();
    run_migrations(&conn, MIGRATIONS).expect("v32 再実行");
    let same_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_sessions WHERE agent_id = speaker_id",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(same_before, same_after, "2 回目で付け替えが二重に起きない");
    assert_eq!(
        agent_of(id_discord),
        recipient,
        "再実行後も (1) は受信側のまま"
    );
    assert_eq!(
        agent_of(id_unresolved),
        "sender-u",
        "再実行後も復元不能行は不変"
    );
}
