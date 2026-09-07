
/// §9A 会話レンダリング（u/e/c 短縮参照・識別子排除・長文切り詰め）の固定。
#[cfg(test)]
mod render_refs_tests {
    use super::*;
    use opencrab_db::queries::SessionLogRow;

    fn speech(agent: &str, speaker: &str, text: &str, origin: Option<&str>) -> SessionLogRow {
        SessionLogRow {
            id: None,
            agent_id: agent.to_string(),
            session_id: "s".into(),
            log_type: "speech".into(),
            content: text.to_string(),
            speaker_id: Some(speaker.to_string()),
            turn_number: None,
            metadata_json: origin.map(|o| serde_json::json!({ "external_origin": o }).to_string()),
            created_at: None,
        }
    }

    fn tool_call(agent: &str, ids: &[&str]) -> SessionLogRow {
        let calls: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| serde_json::json!({"id": id, "function": {"name": "reply", "arguments": "{}"}}))
            .collect();
        let tcj = serde_json::Value::Array(calls).to_string();
        SessionLogRow {
            id: Some(1),
            agent_id: agent.to_string(),
            session_id: "s".into(),
            log_type: "tool_call".into(),
            content: String::new(),
            speaker_id: Some(agent.to_string()),
            turn_number: None,
            metadata_json: Some(serde_json::json!({ "tool_calls_json": tcj }).to_string()),
            created_at: None,
        }
    }

    fn tool_result(agent: &str, id: &str) -> SessionLogRow {
        SessionLogRow {
            id: Some(2),
            agent_id: agent.to_string(),
            session_id: "s".into(),
            log_type: "tool_result".into(),
            content: "ok".into(),
            speaker_id: Some(agent.to_string()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({"tool_call_id": id, "tool_name": "reply"}).to_string(),
            ),
            created_at: None,
        }
    }

    #[test]
    fn speakers_numbered_by_first_appearance_self_has_no_u() {
        let logs = vec![
            speech("me", "pk_alice", "hi", Some("nostr:event:v1:default:e1")),
            speech("me", "me", "hello", None),
            speech("me", "pk_bob", "yo", Some("nostr:event:v1:default:e2")),
            speech("me", "pk_alice", "again", Some("nostr:event:v1:default:e3")),
        ];
        let refs = ConversationRefs::build(&logs, "me");
        assert_eq!(refs.speaker_label("pk_alice"), "u1");
        assert_eq!(refs.speaker_label("pk_bob"), "u2");
        // 自分は u 番号なし（生の agent_id のまま = 名前だけの位置づけ）。
        assert_eq!(refs.speaker_label("me"), "me");
        // 未知話者は生のまま。
        assert_eq!(refs.speaker_label("pk_carol"), "pk_carol");
    }

    #[test]
    fn events_numbered_per_origin_first_appearance() {
        let logs = vec![
            speech("me", "pk_a", "a", Some("nostr:event:v1:default:AAA")),
            speech("me", "pk_b", "b", Some("nostr:event:v1:watch:7:BBB")),
        ];
        let refs = ConversationRefs::build(&logs, "me");
        let a = format_single_log_with_echo(&logs[0], None, Some(&refs));
        assert!(a.starts_with("[u1]e1:"), "{a}");
        let b = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(b.starts_with("[u2]e2:"), "{b}");
        // 生 ID（npub/note/hex/origin）は会話へ出さない。
        assert!(!a.contains("AAA") && !a.contains("nostr:event"));
    }

    #[test]
    fn numbers_are_stable_when_new_logs_arrive() {
        let mut logs = vec![speech("me", "pk_a", "a", Some("nostr:event:v1:default:X"))];
        let first = ConversationRefs::build(&logs, "me");
        assert_eq!(first.speaker_label("pk_a"), "u1");
        logs.push(speech("me", "pk_b", "b", Some("nostr:event:v1:default:Y")));
        let second = ConversationRefs::build(&logs, "me");
        // 既存の番号は不変（初出順は append-only で安定）。
        assert_eq!(second.speaker_label("pk_a"), "u1");
        assert_eq!(second.speaker_label("pk_b"), "u2");
    }

    #[test]
    fn tool_calls_and_results_share_c_numbers() {
        let logs = vec![
            tool_call("me", &["call_aaa", "call_bbb"]),
            tool_result("me", "call_aaa"),
        ];
        let refs = ConversationRefs::build(&logs, "me");
        let call_render = format_single_log_with_echo(&logs[0], None, Some(&refs));
        assert!(call_render.contains("[c1]: reply("), "{call_render}");
        assert!(call_render.contains("[c2]: reply("), "{call_render}");
        assert!(!call_render.contains("call_aaa"));
        let result_render = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(result_render.contains("[c1]:"), "{result_render}");
        assert!(!result_render.contains("call_aaa"));
    }

    #[test]
    fn timeline_items_truncate_at_200_direct_at_2000() {
        let long = "あ".repeat(5000);
        let tl = speech("me", "pk_a", &long, Some("nostr:event:v1:watch:3:E"));
        let refs = ConversationRefs::build(std::slice::from_ref(&tl), "me");
        let out = format_single_log_with_echo(&tl, None, Some(&refs));
        assert!(out.contains("…(全5000字)"), "{}", &out[..out.len().min(80)]);
        // 200 字 + マーカー。元 5000 字は載らない。
        assert!(out.chars().count() < 400);

        let direct = speech("me", "pk_a", &long, Some("nostr:event:v1:default:E"));
        let refs2 = ConversationRefs::build(std::slice::from_ref(&direct), "me");
        let out2 = format_single_log_with_echo(&direct, None, Some(&refs2));
        assert!(out2.contains("…(全5000字)"));
        // 2000 字保持（自分宛て）。
        assert!(out2.chars().count() > 2000);
    }

    // 裁定 A（統一が正・2026-08-31）: §9A の切り詰め + e番号採番は platform 非依存で全 gateway kind に
    // 適用する。web（非 nostr）受信も対象で、>2000 字は `…(全N字)` に切り詰められ eN が付く。
    // external_origin を全 kind で記録する core 汎化（extgate inbound）の web 側の期待描画を固定し、
    // 「web だけ切り詰め・採番されない」旧挙動へ退行しないことを保証する。
    #[test]
    fn web_origin_is_truncated_at_2000_and_e_numbered() {
        let long = "あ".repeat(2500);
        // web の外部 origin（watch でないので自分宛て相当の 2000 字閾値）。
        let ev = speech("bot", "web-user-x", &long, Some("web:conv:v1:abc"));
        let refs = ConversationRefs::build(std::slice::from_ref(&ev), "bot");
        let out = format_single_log_with_echo(&ev, None, Some(&refs));
        // e番号が付く（外部話者 u1・受信 e1）。
        assert!(out.contains("e1"), "web 受信に e番号が付かない: {out}");
        // 2000 字で切り詰め、末尾に全文字数マーカー。
        assert!(out.contains("…(全2500字)"), "切り詰めマーカーが無い: {out}");
        assert!(out.chars().count() < 2100, "2000 字に切り詰まっていない");
        // 生 origin は会話へ出さない。
        assert!(!out.contains("web:conv:v1:abc"), "生 origin 露出: {out}");
    }

    #[test]
    fn self_speech_is_not_truncated() {
        let long = "x".repeat(5000);
        let mine = speech("me", "me", &long, None);
        let refs = ConversationRefs::build(std::slice::from_ref(&mine), "me");
        let out = format_single_log_with_echo(&mine, None, Some(&refs));
        assert!(!out.contains("…(全"));
    }

    #[test]
    fn none_refs_keeps_legacy_rendering() {
        let ev = speech("me", "pk_a", "hi", Some("nostr:event:v1:default:E"));
        let out = format_single_log_with_echo(&ev, None, None);
        // refs なしは従来の生表示（u/e 番号なし）。
        assert!(out.starts_with("[pk_a]"), "{out}");
    }

    /// §9A.1 / row292: DI operation（reply）の tool_call は完了後も arguments（本文）が
    /// 会話へ verbatim 残る。nostr_run 時代の本文喪失の再発防止。preserve_arg_call_ids を
    /// 付けない同一 call は従来どおり →log 参照へ短縮され本文が消えることを対照で示す。
    fn reply_tool_call(preserve: bool) -> SessionLogRow {
        let tcj = serde_json::json!([{
            "id": "call_reply1",
            "function": {"name": "reply", "arguments": "{\"event\":\"e3\",\"text\":\"次ターンに残るべき本文\"}"}
        }])
        .to_string();
        let meta = if preserve {
            serde_json::json!({"tool_calls_json": tcj, "preserve_arg_call_ids": ["call_reply1"]})
        } else {
            serde_json::json!({ "tool_calls_json": tcj })
        };
        SessionLogRow {
            id: Some(7),
            agent_id: "me".into(),
            session_id: "s".into(),
            log_type: "tool_call".into(),
            content: String::new(),
            speaker_id: Some("me".into()),
            turn_number: None,
            metadata_json: Some(meta.to_string()),
            created_at: None,
        }
    }

    #[test]
    fn di_reply_body_survives_digest_next_turn() {
        // call は完了済み（次ターン相当）。
        let completed: std::collections::HashSet<String> =
            std::iter::once("call_reply1".to_string()).collect();

        // preserve あり: 本文が残る（digest されない）。
        let kept = format_single_log_with_echo(&reply_tool_call(true), Some(&completed), None);
        assert!(
            kept.contains("次ターンに残るべき本文"),
            "DI reply 本文が次ターンで消えている: {kept}"
        );

        // 対照（preserve なし）: 従来どおり digest されて本文が消える。
        let lost = format_single_log_with_echo(&reply_tool_call(false), Some(&completed), None);
        assert!(
            !lost.contains("次ターンに残るべき本文"),
            "preserve なしなら →log 短縮で消えるはず（対照）: {lost}"
        );
    }

    // row294b 追修 1/2: 表示時に legacy メタ行・種別ラベル行・生識別子を剥がす。
    #[test]
    fn strips_legacy_meta_line_and_raw_ids_at_display() {
        let npub = format!("npub1{}", "q".repeat(58));
        let note = format!("note1{}", "p".repeat(58));
        let body = format!("こんにちは\n[Nostr kind:1 メンション from={npub} target={note}]");
        let ev = speech("me", "pk_a", &body, Some("nostr:event:v1:default:E"));
        let refs = ConversationRefs::build(std::slice::from_ref(&ev), "me");
        let out = format_single_log_with_echo(&ev, None, Some(&refs));
        assert!(out.contains("こんにちは"), "本文は残す: {out}");
        assert!(
            !out.contains("[Nostr kind:"),
            "種別ラベル行を出さない: {out}"
        );
        assert!(
            !out.contains(&npub) && !out.contains(&note),
            "生 ID を出さない: {out}"
        );
        assert!(!out.contains("from=") && !out.contains("target="), "{out}");
    }

    #[test]
    fn strips_new_9a_label_line_at_display() {
        // 新 §9A 形（from=/target= 無し）でもラベル行は表示に出さない。
        let ev = speech(
            "me",
            "pk_a",
            "やあ\n[Nostr kind:1 メンション]",
            Some("nostr:event:v1:default:E"),
        );
        let refs = ConversationRefs::build(std::slice::from_ref(&ev), "me");
        let out = format_single_log_with_echo(&ev, None, Some(&refs));
        assert!(out.contains("やあ"), "{out}");
        assert!(!out.contains("[Nostr kind:"), "{out}");
        assert!(!out.contains("メンション"), "ラベル語も残さない: {out}");
    }

    // row339: 発言本文（全話者・全 kind）の識別子は**原文のまま**渡す。本文の識別子改変は相手の
    // 発言の書き換え＝情報破壊。以前は bech32/64hex を `<npub…>`/`<id…>` へ短縮していたが撤去した。
    // 構造ラベル行の除去（`[… kind:N …]`）と長文切り詰めは不変（別テストで固定）。
    #[test]
    fn keeps_bare_identifiers_in_body_verbatim() {
        let pubkey = "b".repeat(64); // 64hex
        let npub = format!("npub1{}", "q".repeat(58));
        let body = format!("引用: {npub} と {pubkey} と call_abc123");
        let ev = speech("me", "pk_a", &body, Some("nostr:event:v1:default:E"));
        let refs = ConversationRefs::build(std::slice::from_ref(&ev), "me");
        let out = format_single_log_with_echo(&ev, None, Some(&refs));
        assert!(out.contains(&pubkey), "64hex を原文のまま: {out}");
        assert!(out.contains(&npub), "bech32 を原文のまま: {out}");
        assert!(
            !out.contains("<npub…>") && !out.contains("<id…>"),
            "本文をマスクしていないこと: {out}"
        );
        assert!(out.contains("call_abc123"), "call_ も原文のまま: {out}");
    }

    #[test]
    fn strip_meta_is_display_only_not_stored() {
        // strip はレンダリング専用。ログ本文（保存データ相当）は変更しない。
        let body = "本文\n[Nostr kind:1 メンション]".to_string();
        let ev = speech("me", "pk_a", &body, Some("nostr:event:v1:default:E"));
        assert_eq!(ev.content, body, "保存データは書き換えない");
    }

    // row295c: 自分の話者行は UUID でなく名前（agents.name）。
    #[test]
    fn self_speaker_shows_name_not_uuid() {
        let logs = vec![speech("agent-uuid-xyz", "agent-uuid-xyz", "やあ", None)];
        let mut refs = ConversationRefs::build(&logs, "agent-uuid-xyz");
        refs.set_agent_name("くらぶ");
        let out = format_single_log_with_echo(&logs[0], None, Some(&refs));
        assert!(out.starts_with("[くらぶ]"), "{out}");
        assert!(!out.contains("agent-uuid-xyz"), "生 UUID が残存: {out}");
    }

    // row295b: 括弧間スペースを出さない（ts あり）。
    #[test]
    fn header_has_no_spaces_between_brackets() {
        let mut ev = speech("me", "pk_a", "hi", Some("nostr:event:v1:default:E"));
        ev.created_at = Some("2026-08-30T11:14:42+00:00".into());
        let refs = ConversationRefs::build(std::slice::from_ref(&ev), "me");
        let out = format_single_log_with_echo(&ev, None, Some(&refs));
        assert!(out.starts_with("[u1][2026-08-30 11:14:42]e1:"), "{out}");
        assert!(!out.contains("] ["), "括弧間スペース: {out}");
    }

    // row295c: リプライ/リアクション/リポストは関係注記を残す（素の投稿は注記なし）。
    #[test]
    fn reply_and_reaction_get_relation_annotation() {
        let reply = speech(
            "me",
            "pk_a",
            "そうだね\n[Nostr kind:1 リプライ]",
            Some("nostr:event:v1:default:R"),
        );
        let reaction = speech(
            "me",
            "pk_a",
            "🫧\n[Nostr kind:7 リアクション]",
            Some("nostr:event:v1:default:X"),
        );
        let plain = speech(
            "me",
            "pk_a",
            "こんにちは\n[Nostr kind:1 メンション]",
            Some("nostr:event:v1:default:M"),
        );
        let logs = vec![reply, reaction, plain];
        let refs = ConversationRefs::build(&logs, "me");
        let o_reply = format_single_log_with_echo(&logs[0], None, Some(&refs));
        assert!(o_reply.contains("(reply→外部):"), "{o_reply}");
        assert!(
            !o_reply.contains("[Nostr kind:"),
            "ラベル行は出さない: {o_reply}"
        );
        let o_reaction = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(o_reaction.contains("(reaction→外部):"), "{o_reaction}");
        assert!(o_reaction.contains("🫧"), "本文が残る: {o_reaction}");
        let o_plain = format_single_log_with_echo(&logs[2], None, Some(&refs));
        assert!(!o_plain.contains("→外部"), "素の投稿に注記なし: {o_plain}");
    }

    // C6（発話クラス化）: 自分の発話 op（reply/reaction/repost）は本文＋関係注記のみで残り、
    // 機械行（[tool_call]/[tool_result]）を出さない。注記は metadata 由来（utterance_kind）。
    #[test]
    fn outgoing_utterance_renders_body_and_relation_without_machine_lines() {
        let target_id = "ab".repeat(32);
        let target = speech(
            "me",
            "pk_other",
            "元の投稿",
            Some(&format!("nostr:event:v1:default:{target_id}")),
        );
        let mut my_reply = speech("me", "me", "そうだね", None);
        my_reply.metadata_json = Some(
            serde_json::json!({
                "source": "external_reply",
                "utterance_kind": "reply",
                "reply_target": target_id,
            })
            .to_string(),
        );
        let mut my_reaction = speech("me", "me", "🫧", None);
        my_reaction.metadata_json = Some(
            serde_json::json!({ "utterance_kind": "reaction", "reply_target": "ff".repeat(32) })
                .to_string(),
        );
        let logs = vec![target, my_reply, my_reaction];
        let refs = ConversationRefs::build(&logs, "me");
        let o_reply = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(
            o_reply.contains("(reply→e1):"),
            "対象は会話内 e1: {o_reply}"
        );
        assert!(o_reply.contains("そうだね"), "本文が残る: {o_reply}");
        assert!(
            !o_reply.contains("[tool_call]") && !o_reply.contains("[tool_result]"),
            "機械行を出さない: {o_reply}"
        );
        let o_reaction = format_single_log_with_echo(&logs[2], None, Some(&refs));
        // 対象 ff… は会話に無いので →外部。
        assert!(o_reaction.contains("(reaction→外部):"), "{o_reaction}");
        assert!(o_reaction.contains("🫧"), "本文が残る: {o_reaction}");
    }

    // row295c 6b: reply_target が記録されていれば会話内の e 番号へ解決する。
    #[test]
    fn reply_target_resolves_to_e_number_when_recorded() {
        let target_id = "cc".repeat(32);
        let target = speech(
            "me",
            "pk_a",
            "元投稿",
            Some(&format!("nostr:event:v1:default:{target_id}")),
        );
        let mut reply = speech(
            "me",
            "pk_b",
            "そうだね\n[Nostr kind:1 リプライ]",
            Some(&format!("nostr:event:v1:default:{}", "dd".repeat(32))),
        );
        reply.metadata_json = Some(
            serde_json::json!({
                "external_origin": format!("nostr:event:v1:default:{}", "dd".repeat(32)),
                "reply_target": target_id,
            })
            .to_string(),
        );
        let logs = vec![target, reply];
        let refs = ConversationRefs::build(&logs, "me");
        let out = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(
            out.contains("(reply→e1)"),
            "対象が e 番号解決されない: {out}"
        );
    }

    // row295d: 凍結 snapshot blob の UUID / call_ / digest hex を除去・短縮する。
    #[test]
    fn frozen_snapshot_elides_uuid_call_and_digest() {
        let blob = "[me][2026-08-30 06:06:45]:\n[tool_call]:\n[c1]: execute_shell({\"ref\":\"log:1\",\"digest\":\"15e51315716f5bc7\",\"bytes\":116})\ncall_XH2Y1M9nLDkUzHxvC3J2RLCb → spawned subtask df58ec83-960c-45e3-b69c-ff493b133afc";
        let out = strip_frozen_snapshot(blob);
        assert!(
            !out.contains("df58ec83-960c-45e3-b69c-ff493b133afc"),
            "UUID 残存: {out}"
        );
        assert!(out.contains("<uuid…>"), "{out}");
        assert!(
            !out.contains("call_XH2Y1M9nLDkUzHxvC3J2RLCb"),
            "call_ 残存: {out}"
        );
        assert!(out.contains("<call…>"), "{out}");
        assert!(!out.contains("15e51315716f5bc7"), "digest hex 残存: {out}");
        assert!(
            out.contains("\"digest\":\"…\""),
            "digest 短縮形が無い: {out}"
        );
        // 新形式（→log 参照・c 番号）は保持。
        assert!(out.contains("log:1") && out.contains("[c1]"), "{out}");
    }

    // row295d 変種: dashed session id（`nostr-<uuid>-<channel>`）に埋まった UUID も剥がす。
    #[test]
    fn frozen_snapshot_elides_uuid_embedded_in_dashed_session_id() {
        let blob = "session=nostr-33196264-5908-4f04-b24a-efd7aa6d2014-caldera へ完了";
        let out = strip_frozen_snapshot(blob);
        assert!(
            !out.contains("33196264-5908-4f04-b24a-efd7aa6d2014"),
            "埋め込み UUID 残存: {out}"
        );
        assert!(out.contains("<uuid…>"), "{out}");
        // 周辺（session=nostr-…-caldera）は残ってよい（生 UUID だけ落とす）。
        assert!(out.contains("caldera") && out.contains("完了"), "{out}");
    }

    // 単一ログ経路（per-log）は UUID/call_/digest を触らない（利用者本文の過剰除去を避ける）。
    #[test]
    fn per_log_strip_leaves_uuid_untouched() {
        let body = "予約番号は df58ec83-960c-45e3-b69c-ff493b133afc です";
        assert_eq!(strip_inbound_meta_for_display(body), body);
    }

    // row339: v2 マーカー付き snapshot（新規凍結）は read 時にスクラブせず本文原文のまま復元する。
    #[test]
    fn restore_v2_snapshot_keeps_body_verbatim() {
        let uuid = "df58ec83-960c-45e3-b69c-ff493b133afc";
        let npub = format!("npub1{}", "q".repeat(58));
        // 生成元は既にクリーン（構造=u/e 短縮参照・本文=原文）。
        let clean = format!("[u1][2026-08-30 06:06:45]e1:\n予約 {uuid} と {npub}");
        let blob = frozen_snapshot_with_marker(&clean);
        let out = restore_frozen_snapshot(&blob);
        assert_eq!(
            out, clean,
            "v2 は本文原文のまま復元（マーカーも除去）: {out}"
        );
        assert!(out.contains(uuid) && out.contains(&npub), "{out}");
        assert!(
            !out.contains("<uuid…>") && !out.contains("<npub…>"),
            "v2 本文が再マスクされた: {out}"
        );
    }

    // row339: マーカー無しの legacy blob（載せ替え前の歴史データ）は従来どおりスクラブする。
    #[test]
    fn restore_legacy_snapshot_still_scrubs() {
        let blob = "session=nostr-33196264-5908-4f04-b24a-efd7aa6d2014-caldera へ完了";
        let out = restore_frozen_snapshot(blob);
        assert_eq!(
            out,
            strip_frozen_snapshot(blob),
            "legacy は従来スクラブと同一経路"
        );
        assert!(
            out.contains("<uuid…>"),
            "legacy UUID がスクラブされない: {out}"
        );
    }

    // row339: 凍結（マーカー付与）→復元の往復で本文が保存される（自己治癒ループの冪等性）。
    #[test]
    fn frozen_snapshot_marker_roundtrip_is_lossless() {
        let clean = "[u1][2026-08-30 06:06:45]e1:\nrunId: e059e80f-960c-45e3-b69c-ff493b133afc";
        assert_eq!(
            restore_frozen_snapshot(&frozen_snapshot_with_marker(clean)),
            clean
        );
    }

    // row295b: subtask_completed は s 番号ヘッダ＋result 本文のみ（生 UUID/定型 field を出さない）。
    #[test]
    fn subtask_completed_uses_s_number_not_uuid() {
        let spawn = SessionLogRow {
            id: Some(1),
            agent_id: "me".into(),
            session_id: "s".into(),
            log_type: "tool_result".into(),
            content: r#"{"success":true,"data":{"subtask_id":"sub-xyz-1","status":"spawned"}}"#
                .into(),
            speaker_id: Some("me".into()),
            turn_number: None,
            metadata_json: Some(r#"{"tool_call_id":"tc-1","tool_name":"spawn_subtask"}"#.into()),
            created_at: None,
        };
        let done = SessionLogRow {
            id: Some(2),
            agent_id: "me".into(),
            session_id: "s".into(),
            log_type: "system".into(),
            content: r#"{"type":"subtask_completed","subtask_id":"sub-xyz-1","session_id":"subtask-sub-xyz-1","exit_reason":"completed","result":"調査おわり"}"#.into(),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        let logs = vec![spawn, done];
        let refs = ConversationRefs::build(&logs, "me");
        let out = format_single_log_with_echo(&logs[1], None, Some(&refs));
        assert!(out.contains("[s1 完了]"), "s 番号ヘッダが無い: {out}");
        assert!(out.contains("調査おわり"), "result 本文が残る: {out}");
        assert!(!out.contains("sub-xyz-1"), "生 UUID が残存: {out}");
        assert!(!out.contains("exit_reason"), "定型 field が残存: {out}");
    }
}
