
/// heartbeat 指示文は会話へ積まない（#501）。
///
/// 以前は `scheduler.rs::run_one_heartbeat` が発火のたびに同一文面の指示文（`system` /
/// `speaker_id='heartbeat'`）をセッションログへ挿入し、それが全件そのまま会話へ復元されて
/// いた。本番の heartbeat セッションでは同一指示が 192 件並び、「同じ指示 → IDLE」の対を
/// 何十回も見せて挙動を歪めていた。#501 で指示文は system プロンプトへ移した
/// （`scheduler::run_one_heartbeat`）ので、会話再構成では指示文 scaffolding を**全件落とす**。
/// subtask 完了本文（`system` / `speaker_id=None`, #404 / #405）は落とさない。
#[cfg(test)]
mod heartbeat_prompt_dedup_tests {
    use super::{build_conversation_string, retain_conversation_logs};

    const AGENT: &str = "a1";
    const HB_SESSION: &str = "heartbeat-a1-222";

    /// 毎 tick 挿入されていた指示文（本番と同形）。#501 以降は書かれないが、既存 DB には残る。
    const HB_PROMPT: &str = "[ハートビート] 現在の会話「（自律ハートビート）」。20分ごとに巡回して新着に反応する。\n出力形式: SPEAK/LEARN/IDLE のいずれか。";

    fn insert(conn: &rusqlite::Connection, log_type: &str, speaker: Option<&str>, content: &str) {
        opencrab_db::queries::insert_session_log(
            conn,
            &opencrab_db::queries::SessionLogRow {
                id: None,
                agent_id: AGENT.to_string(),
                session_id: HB_SESSION.to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: speaker.map(|s| s.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            },
        )
        .unwrap();
    }

    /// 既存 DB に積まれた指示文（複数件）が会話組み立ての出力に**一切現れない**こと。
    /// 落とす filter を戻すと 3 回現れて赤くなる（恒真ではない）。
    #[test]
    fn heartbeat_prompts_never_appear_in_the_conversation() {
        let conn = opencrab_db::init_memory().unwrap();
        // 3 tick 分の（過去の）指示文と、その間の発話・subtask 完了本文を積む。
        insert(&conn, "system", Some("heartbeat"), HB_PROMPT);
        insert(&conn, "speech", Some("owner"), "新着あった？");
        insert(&conn, "system", Some("heartbeat"), HB_PROMPT);
        insert(&conn, "speech", Some(AGENT), "SPEAK: ありました");
        // subtask 完了本文（#404 / #405）: speaker_id=None なので落としてはならない。
        insert(
            &conn,
            "system",
            None,
            r#"{"type":"subtask_completed","subtask_id":"st-1","result":"調査おわり"}"#,
        );
        insert(&conn, "system", Some("heartbeat"), HB_PROMPT);

        let out = build_conversation_string(&conn, HB_SESSION, AGENT, 100_000).unwrap();

        assert_eq!(
            out.matches("20分ごとに巡回して新着に反応する").count(),
            0,
            "heartbeat 指示文が会話へ復元されている（system プロンプトへ移したはず）: {out}"
        );
        // #404 / #405: subtask 完了本文は残る。
        assert!(
            out.contains("調査おわり"),
            "subtask 完了本文が落ちた: {out}"
        );
        // 発話は両方残る（除外が効きすぎていないこと）。
        assert!(
            out.contains("新着あった？") && out.contains("ありました"),
            "発話が落ちた: {out}"
        );
    }

    /// `retain_conversation_logs` は指示文 scaffolding を全件落とし、subtask 完了本文
    /// （speaker=None）と発話は残す。
    #[test]
    fn retain_drops_all_scaffolds_keeps_completion_and_speech() {
        let mk = |id: i64, log_type: &str, speaker: Option<&str>, content: &str| {
            opencrab_db::queries::SessionLogRow {
                id: Some(id),
                agent_id: AGENT.to_string(),
                session_id: HB_SESSION.to_string(),
                log_type: log_type.to_string(),
                content: content.to_string(),
                speaker_id: speaker.map(|s| s.to_string()),
                turn_number: None,
                metadata_json: None,
                created_at: None,
            }
        };
        let logs = vec![
            mk(1, "system", Some("heartbeat"), "指示v1"),
            mk(2, "speech", Some(AGENT), "SPEAK: やあ"),
            mk(3, "system", Some("heartbeat"), "指示v2"),
            mk(
                4,
                "system",
                None,
                r#"{"type":"subtask_completed","result":"r"}"#,
            ),
        ];
        let kept = retain_conversation_logs(logs);
        // 指示文 scaffolding は 1 件も残らない。
        assert!(
            !kept.iter().any(
                |l| l.speaker_id.as_deref() == Some(opencrab_db::queries::HEARTBEAT_SPEAKER_ID)
            ),
            "指示文 scaffolding が残った"
        );
        // subtask 完了本文（speaker=None）と発話は残る。
        assert!(
            kept.iter().any(|l| l.speaker_id.is_none()),
            "subtask 完了本文が落ちた"
        );
        assert!(
            kept.iter().any(|l| l.content == "SPEAK: やあ"),
            "発話が落ちた"
        );
    }
}
