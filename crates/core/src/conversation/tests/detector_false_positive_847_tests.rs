
/// #847: 漏れ検知器の偽陽性（利用者発話本文に UUID/call_id が含まれると描画が正しくても WARN）と、
/// 真陽性（構造部分＝話者行・tool_call に生 ID が残る本物の描画器バグ）の維持を固定する。
#[cfg(test)]
mod detector_false_positive_847_tests {
    use super::{
        format_single_log_with_echo, leaked_identifier_in_delta, leaked_identifier_in_render,
        ConversationRefs,
    };
    use opencrab_db::queries::SessionLogRow;

    const AGENT: &str = "33196264-5908-4f04-b24a-efd7aa6d2014";
    // `call_` の後ろ 16 桁以上の英数（scrub の elide_call_ids 閾値）。大小混在で elide_raw には
    // 当たらない（＝speech 本文は保持され、full スクラブ基準の旧検知器だけが鳴る）。
    const RAW_CALL: &str = "call_abcdef0123456789ABCDEF";

    fn user_speech(content: &str) -> SessionLogRow {
        SessionLogRow {
            id: Some(1),
            agent_id: AGENT.to_string(),
            session_id: "s".to_string(),
            log_type: "speech".to_string(),
            content: content.to_string(),
            speaker_id: Some("pubkey-user".to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: Some("2026-08-31T15:00:00Z".to_string()),
        }
    }

    /// 偽陽性が消える: 利用者の発話本文に生 UUID / call_id が含まれても WARN は出ない。
    /// 描画は本文を保持（elide_raw のみ・UUID/call_ は残す）しており描画は正しい。旧検知器
    /// （本文込み・full スクラブ基準）なら鳴っていたことも同時に固定し、実際に偽陽性を潰したことを示す。
    #[test]
    fn user_speech_with_raw_identifiers_does_not_warn() {
        let uuid = "df1bc106-960c-45e3-b69c-ff493b133afc";
        let log = user_speech(&format!("この件は {uuid} と {RAW_CALL} を参照して"));
        let refs = ConversationRefs::build(std::slice::from_ref(&log), AGENT);
        let rendered = format_single_log_with_echo(&log, None, Some(&refs));

        // 描画は本文をそのまま保持（描画は正しい）。
        assert!(
            rendered.contains(uuid),
            "本文の UUID が保持されていない: {rendered}"
        );
        assert!(
            rendered.contains(RAW_CALL),
            "本文の call_id が保持されていない: {rendered}"
        );
        // 新検知器: 偽陽性なし。
        assert!(
            leaked_identifier_in_render(&log, &rendered).is_none(),
            "利用者本文の生 ID で偽 WARN が出た: {rendered}"
        );
        // 回帰前提: 旧検知器（本文込み）なら鳴っていた＝これは本当の偽陽性だった。
        assert!(
            leaked_identifier_in_delta(&rendered).is_some(),
            "回帰テストの前提が崩れた（旧検知器も沈黙）: {rendered}"
        );
    }

    /// 真陽性は維持（話者行）: 自分の speech で表示名を引けない（set_agent_name なし＝get_agent 失敗相当）と
    /// speaker_label が生 agent UUID へフォールバックし話者行へ載る。これは本物の描画器バグで検知し続ける。
    #[test]
    fn leaked_raw_uuid_in_speaker_header_still_warns() {
        let log = SessionLogRow {
            speaker_id: Some(AGENT.to_string()),
            ..user_speech("本文はここ")
        };
        let refs = ConversationRefs::build(std::slice::from_ref(&log), AGENT);
        let rendered = format_single_log_with_echo(&log, None, Some(&refs));
        assert!(
            rendered.contains(AGENT),
            "前提: 話者行に生 agent UUID が載る: {rendered}"
        );
        assert!(
            leaked_identifier_in_render(&log, &rendered).is_some(),
            "話者行の生 UUID 漏れを検知しそこねた: {rendered}"
        );
    }

    /// 真陽性は維持（tool_call）: refs.call_of が引けない（未採番）と `id=call_…` フォールバックで
    /// 生 call_id が構造部分へ残る。speech 以外は全行が検知対象なので従来どおり鳴る。
    #[test]
    fn leaked_raw_call_id_in_tool_call_still_warns() {
        let tool_call = SessionLogRow {
            id: Some(2),
            agent_id: AGENT.to_string(),
            session_id: "s".to_string(),
            log_type: "tool_call".to_string(),
            content: "call".to_string(),
            speaker_id: Some(AGENT.to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    "tool_calls_json": serde_json::json!([{
                        "id": RAW_CALL,
                        "function": {"name": "spawn_subtask", "arguments": "{}"}
                    }])
                    .to_string()
                })
                .to_string(),
            ),
            created_at: Some("2026-08-31T15:00:00Z".to_string()),
        };
        // 空ログから refs を作る → この call は未採番 → `id=call_…` フォールバック。
        let empty: [SessionLogRow; 0] = [];
        let refs = ConversationRefs::build(&empty, AGENT);
        let rendered = format_single_log_with_echo(&tool_call, None, Some(&refs));
        assert!(
            rendered.contains(RAW_CALL),
            "前提: tool_call に生 call_id が載る: {rendered}"
        );
        assert!(
            leaked_identifier_in_render(&tool_call, &rendered).is_some(),
            "tool_call の生 call_id 漏れを検知しそこねた: {rendered}"
        );
    }

    /// #847 follow-up 偽陽性が消える: preserve_arg_call_ids の DI reply 引数本文（verbatim 保持）に
    /// 生 64hex（返信先 event_id 等）が含まれても WARN は出ない。call_ref は c1（構造は clean）で
    /// 引数本文だけに生 ID がある。旧検知器（引数込み・full スクラブ基準）なら鳴っていたことも固定。
    #[test]
    fn tool_call_preserved_verbatim_arg_body_does_not_warn() {
        let event_hex = format!("7be6255f{}", "a".repeat(56)); // 64hex（返信先 event_id 相当）
        let call_id = "call_reply00000000000000"; // 採番されるので call_ref は c1
        let args = format!(r#"{{"reply_to":"{event_hex}","content":"はい"}}"#);
        let tool_call = SessionLogRow {
            id: Some(3),
            agent_id: AGENT.to_string(),
            session_id: "s".to_string(),
            log_type: "tool_call".to_string(),
            content: "call".to_string(),
            speaker_id: Some(AGENT.to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({
                    // DI operation の call は arguments を verbatim 保持（§9A.1/row292）。
                    "preserve_arg_call_ids": [call_id],
                    "tool_calls_json": serde_json::json!([{
                        "id": call_id,
                        "function": {"name": "nostr_run", "arguments": args}
                    }])
                    .to_string()
                })
                .to_string(),
            ),
            created_at: Some("2026-08-31T15:00:00Z".to_string()),
        };
        // refs にこの call を採番させ（call_ref=c1）、自分の表示名も設定して話者行を clean にする
        // （名前引き成功相当・話者行に生 agent UUID を出さない → 生 ID は引数本文だけに残る）。
        let mut refs = ConversationRefs::build(std::slice::from_ref(&tool_call), AGENT);
        refs.set_agent_name("くらぶ");
        // completed かつ preserve → 引数は →log:N に畳まれず verbatim のまま（preserve 経路を厳密に通す）。
        let mut completed = std::collections::HashSet::new();
        completed.insert(call_id.to_string());
        let rendered = format_single_log_with_echo(&tool_call, Some(&completed), Some(&refs));

        // 構造は clean（call_ref は c1・生 call_id は出ない）、引数本文は verbatim 保持で生 64hex が残る。
        assert!(
            rendered.contains("[c1]"),
            "call_ref が c1 でない: {rendered}"
        );
        assert!(
            rendered.contains(&event_hex),
            "preserve 引数本文が verbatim 保持されていない: {rendered}"
        );
        // 新検知器: 引数本文は対象外 → 偽陽性なし。
        assert!(
            leaked_identifier_in_render(&tool_call, &rendered).is_none(),
            "preserve 引数本文の生 ID で偽 WARN が出た: {rendered}"
        );
        // 回帰前提: 旧検知器（引数込み）なら鳴っていた＝これは本当の偽陽性だった。
        assert!(
            leaked_identifier_in_delta(&rendered).is_some(),
            "回帰テストの前提が崩れた（旧検知器も沈黙）: {rendered}"
        );
    }
}
