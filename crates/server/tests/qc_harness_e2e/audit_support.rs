// =====================================================================================
// 監査ピン（test/harness-audit-outermost-pins・#898/#899/#900）
//
// 本セクションは「モック LLM ハーネスの最外層で配送回数・保存件数・LLM 回数・残留マーカー・
// ゲート反応を pin していれば 2026-09-02 QC の劣化を赤で捕まえられた」ことを固定する赤テスト。
// 現 tip（fabd6556）で **赤** になるべきで、各修正 PR がこれを緑にする。実装は行わない。
//
// 観測チャネル（このハーネスが提供するもの）:
//   - 配送: dry-run say バッファ（`captured()` の CapturedSay{kind, body}）。kind は "standalone"
//           /"reply"。回数は body マーカーで filter して count する。
//   - 保存: session_logs（`list_session_logs_by_session`）の log_type=="speech" 行。
//           エージェント自身の発話は speaker_id==AGENT_ID。
//   - LLM 回数: mock 側のカウンタ（system_prompts().len() もしくは AtomicUsize）。
//   - 残留マーカー: say body / speech content に "CONTINUE" / "NO_REPLY" が現れないこと。
//   - 次ターン typed 履歴: 2 ターン目リクエストの Assistant ロールメッセージ本文。
// =====================================================================================

fn agent_speech_contents(core: &Core, session_id: &str) -> Vec<String> {
    let conn = core.extgate.db.lock().unwrap();
    opencrab_db::queries::list_session_logs_by_session(&conn, session_id)
        .unwrap()
        .into_iter()
        .filter(|l| l.log_type == "speech" && l.speaker_id.as_deref() == Some(AGENT_ID))
        .map(|l| l.content)
        .collect()
}

