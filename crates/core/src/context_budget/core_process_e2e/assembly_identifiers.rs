/// (c) 46 往復級の長タスクを checkpoint 無しで圧縮しても、発端と直近 5 speech が残る。
#[test]
fn c_verbatim_window_keeps_origin_and_recent_speech_without_checkpoint() {
    let conn = opencrab_db::init_memory().unwrap();
    let recent = seed_long_task_with_recent_speech(&conn);
    let logs = opencrab_db::queries::list_session_logs_by_session(&conn, SESSION).unwrap();
    let retained = crate::conversation::retain_conversation_logs(logs);

    let items = items_from_logs(&conn, SESSION, AGENT, &retained).unwrap();
    let before: usize = items.iter().map(|i| i.tokens).sum();
    assert!(
        before > HIGH,
        "長タスク fixture が高水位を超えること: before={before}"
    );
    let compacted = compact_to_low_water(&items, HIGH, LOW);
    assert!(compacted.fired, "圧縮が発火すること: before={before}");
    assert_verbatim_window(&compacted.text, &recent);

    let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    let mut gov = TurnGovernor::new(HIGH, LOW);
    let finished = gov
        .finish_turn(
            &conn,
            SESSION,
            &assembled.items,
            assembled.through_log_id,
            &assembled.text,
        )
        .unwrap();
    assert!(finished.fired, "本番 finish_turn でも圧縮する");
    assert_verbatim_window(&finished.text, &recent);

    let started = build_conversation_string_with_waters(&conn, SESSION, AGENT, HIGH, LOW, false)
        .expect("圧縮後の開始組立");
    assert_verbatim_window(&started, &recent);
}

/// (d) 連続投稿しても圧縮が毎回発火しない。実際の finish_turn 回数で数える。
#[test]
fn d_hysteresis_consecutive_posts_do_not_refire() {
    let conn = opencrab_db::init_memory().unwrap();
    seed_over_high(&conn);
    let _ = take_governor_events();

    let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    let mut gov = TurnGovernor::new(HIGH, LOW);
    let first = gov
        .finish_turn(
            &conn,
            SESSION,
            &assembled.items,
            assembled.through_log_id,
            &assembled.text,
        )
        .unwrap();
    assert!(first.fired, "最初の投稿で圧縮する");
    assert!(first.after_tokens <= LOW || first.low_water_unreachable);

    for n in 0..5 {
        insert_speech(&conn, "owner", &format!("post-{n} {}", "z".repeat(80)));
        let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
        let mut gov = TurnGovernor::new(HIGH, LOW);
        let again = gov
            .finish_turn(
                &conn,
                SESSION,
                &assembled.items,
                assembled.through_log_id,
                &assembled.text,
            )
            .unwrap();
        assert!(
            !again.fired,
            "低水位へ戻った後の連続投稿 {n} で再発火: tokens={}",
            assembled.tokens
        );
    }

    let fires = compact_fired_phases(&take_governor_events());
    assert_eq!(
        fires,
        vec![CompactPhase::TurnEnd],
        "ヒステリシスが効かず毎回発火した: {fires:?}"
    );
}

/// row295 実測再現: 旧形式の会話 snapshot blob（§9A/refs 描画経路を通らず連結される）に
/// legacy メタ行・生識別子が残り、新形式 delta と混在する。assemble_from_snapshot が read 時に
/// blob 部も剥がすことを固定する（追修1 が単一ログ描画だけに効いて中間 blob に届かなかった穴）。
///
/// メタ行の transport ラベルは本番では固有名だが、剥がしは汎用マーカー ` kind:<数字>` で行うため、
/// core のテストは R7 境界（no_gate_identifiers）を守り中立ラベル `[inbound kind:… ]` で同じ経路を突く。
#[test]
fn legacy_snapshot_blob_is_stripped_on_assembly() {
    let conn = opencrab_db::init_memory().unwrap();
    let legacy_pubkey = "9f".repeat(32); // 旧話者行の生 64hex（u 番号化されていない）
    let npub = format!("npub1{}", "q".repeat(58));
    let note = format!("note1{}", "p".repeat(58));
    // 旧レンダリング済み blob: 旧話者行 + legacy メタ行（from=/target= 付き）。
    let legacy_blob = format!(
        "[{legacy_pubkey}] [2026-08-30 06:06:45]:\nこんにちは、みなさん\n[inbound kind:1 メンション from={npub} target={note}]"
    );
    let snap = opencrab_db::queries::ConversationSnapshotRow {
        id: None,
        session_id: SESSION.into(),
        compacted_conversation: legacy_blob,
        through_log_id: 0,
        token_count: 100,
        created_at: None,
    };
    opencrab_db::queries::insert_conversation_snapshot(&conn, &snap).unwrap();
    // snapshot 以降の新規行（delta）。§9A/refs 経路で描画され新形式ラベル行を含む。
    let delta = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: AGENT.into(),
        session_id: SESSION.into(),
        log_type: "speech".into(),
        content: "やあ\n[inbound kind:1 メンション]".into(),
        speaker_id: Some("pk_new".into()),
        turn_number: None,
        metadata_json: Some(
            serde_json::json!({"external_origin": "ext:event:v1:default:E9"}).to_string(),
        ),
        created_at: None,
    };
    opencrab_db::queries::insert_session_log(&conn, &delta).unwrap();

    let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    let text = &assembled.text;
    // メタ行・生 ID は blob 部・delta 部とも消える。
    assert!(
        !text.contains("[inbound kind:"),
        "種別ラベル行が残存: {text}"
    );
    assert!(
        !text.contains(&npub) && !text.contains(&note),
        "生 bech32 が残存: {text}"
    );
    assert!(
        !text.contains(&legacy_pubkey),
        "旧話者行の生 64hex が残存: {text}"
    );
    // 本文は blob・delta とも残る。
    assert!(
        text.contains("こんにちは、みなさん"),
        "blob 本文が落ちた: {text}"
    );
    assert!(text.contains("やあ"), "delta 本文が落ちた: {text}");
}

/// row295 item4 実測再現: 並行バッチ（execute_shell×2）が 1 subtask として detach され、call ごとに
/// 同一 subtask_id の spawn 受理 tool_result を重複記録する。assemble が初出だけ表示することを固定。
#[test]
fn parallel_spawn_ack_is_rendered_once() {
    let conn = opencrab_db::init_memory().unwrap();
    // 実 DB と同じ flat 形（data 包み無し・status/subtask_id が top-level）。
    let content = r#"{"label":"execute_shell([...]), execute_shell([\"60\"])","status":"spawned","subtask_id":"dup-sub-1","tool":"execute_shell"}"#;
    for tcid in ["call-a", "call-b"] {
        let row = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: AGENT.into(),
            session_id: SESSION.into(),
            log_type: "tool_result".into(),
            content: content.into(),
            speaker_id: Some(AGENT.into()),
            turn_number: None,
            metadata_json: Some(
                serde_json::json!({"tool_call_id": tcid, "tool_name": "execute_shell"}).to_string(),
            ),
            created_at: None,
        };
        opencrab_db::queries::insert_session_log(&conn, &row).unwrap();
    }
    let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    // row318: spawn 受理は subtask_id を **描画時に** s 番号へ短縮する（生 id を出さない）。
    // dedup（初出のみ）は据え置きなので s 番号は 1 回だけ出る。
    assert_eq!(
        assembled.text.matches("subtask s1 を起動").count(),
        1,
        "spawn 受理が s 番号で 1 回だけ出ない（二重表示 or 未短縮）: {}",
        assembled.text
    );
    assert!(
        !assembled.text.contains("dup-sub-1"),
        "生 subtask_id が残存: {}",
        assembled.text
    );
    assert!(
        !assembled.text.contains("spawned"),
        "定型 status 語が残存: {}",
        assembled.text
    );
}

/// `8-4-4-4-12` hex（UUID 正規表現）の出現数を数える（依存を足さない手書きスキャナ）。
fn count_uuids(s: &str) -> usize {
    let b = s.as_bytes();
    let groups = [8usize, 4, 4, 4, 12];
    let mut count = 0;
    let mut i = 0;
    while i < b.len() {
        let mut pos = i;
        let mut ok = true;
        for (gi, &g) in groups.iter().enumerate() {
            if gi > 0 {
                if b.get(pos) != Some(&b'-') {
                    ok = false;
                    break;
                }
                pos += 1;
            }
            for _ in 0..g {
                match b.get(pos) {
                    Some(c) if c.is_ascii_hexdigit() => pos += 1,
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                break;
            }
        }
        if ok {
            count += 1;
            i = pos;
        } else {
            i += 1;
        }
    }
    count
}

/// row318 完了固定: tool_call（自分）・spawn 受理・凍結 snapshot の**全描画経路**で、描画時に短縮形
/// （[くらぶ]/s 番号/<id…>）を出し、最終会話文字列に生の長識別子（UUID / ダッシュ無し 32hex）が
/// **1 個も**現れないことを固定する。検知器 [`leaked_identifier_in_delta`] も最終文字列で None
/// （＝描画器が短縮形を出し損ねていない）。
#[test]
fn final_conversation_has_zero_raw_identifiers_across_all_render_paths() {
    let conn = opencrab_db::init_memory().unwrap();
    let agent_uuid = "33196264-5908-4f04-b24a-efd7aa6d2014";
    // 自分の表示名を引けるよう agents に登録（build_conversation_refs が get_agent→name）。
    opencrab_db::queries::upsert_agent(
        &conn,
        &opencrab_db::queries::AgentRow {
            agent_id: agent_uuid.into(),
            name: "くらぶ".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "くらぶ".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    // 凍結 snapshot blob（旧描画テキスト・refs 経路を通らない）にダッシュ無し 32hex を仕込む。
    let hex32 = "abcdef0123456789abcdef0123456789"; // 32hex
    let snap = opencrab_db::queries::ConversationSnapshotRow {
        id: None,
        session_id: SESSION.into(),
        compacted_conversation: format!("[くらぶ][2026-08-31 15:00:00]:\n凍結参照 {hex32} を見た"),
        through_log_id: 0,
        token_count: 50,
        created_at: None,
    };
    opencrab_db::queries::insert_conversation_snapshot(&conn, &snap).unwrap();

    // delta1: tool_call（話者＝自分の agent UUID）。描画器が [くらぶ] を出すべき。
    let tool_call = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: agent_uuid.into(),
        session_id: SESSION.into(),
        log_type: "tool_call".into(),
        content: "call".into(),
        speaker_id: Some(agent_uuid.into()),
        turn_number: None,
        metadata_json: Some(
            serde_json::json!({
                "tool_calls_json": serde_json::json!([{
                    "id": "tc-1",
                    "function": {"name": "spawn_subtask", "arguments": "{\"prompt\":\"調べて\"}"}
                }])
                .to_string()
            })
            .to_string(),
        ),
        created_at: None,
    };
    opencrab_db::queries::insert_session_log(&conn, &tool_call).unwrap();

    // delta2: spawn 受理 tool_result（生 subtask UUID）。描画器が s 番号を出すべき。
    let spawn_ack = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: agent_uuid.into(),
        session_id: SESSION.into(),
        log_type: "tool_result".into(),
        content: r#"{"status":"spawned","subtask_id":"df1bc106-960c-45e3-b69c-ff493b133afc","tool":"spawn_subtask"}"#
            .into(),
        speaker_id: Some(agent_uuid.into()),
        turn_number: None,
        metadata_json: Some(
            serde_json::json!({"tool_call_id": "tc-1", "tool_name": "spawn_subtask"}).to_string(),
        ),
        created_at: None,
    };
    opencrab_db::queries::insert_session_log(&conn, &spawn_ack).unwrap();

    let assembled = assemble_from_snapshot(&conn, SESSION, agent_uuid).unwrap();
    let text = &assembled.text;

    // 完了条件: 最終会話文字列に UUID が 0 個。
    assert_eq!(count_uuids(text), 0, "生 UUID が残存: {text}");
    // ダッシュ無し 32hex も 0 個（snapshot 経路の残存3）。
    assert!(!text.contains(hex32), "ダッシュ無し 32hex が残存: {text}");
    // 自分の tool_call 話者行は名前（<uuid…> プレースホルダでも生 UUID でもない）。
    assert!(
        text.contains("[くらぶ]"),
        "自分の話者行が名前でない: {text}"
    );
    assert!(
        !text.contains(agent_uuid),
        "自分の生 agent UUID が残存: {text}"
    );
    // spawn 受理は s 番号で描画（生 subtask UUID を出さない）。
    assert!(
        text.contains("subtask s1 を起動"),
        "spawn 受理が s 番号で描画されていない: {text}"
    );
    assert!(!text.contains("df1bc106"), "生 subtask UUID が残存: {text}");
    // 検知器: 最終文字列に生識別子の取りこぼしなし（描画器が全経路で短縮形を出した）。
    assert!(
        crate::conversation::leaked_identifier_in_delta(text).is_none(),
        "検知器が生識別子を検出（描画器バグ・短縮形が出ていない）: {text}"
    );
}

/// row318 実データ漏れ（検知器が本番で捕捉・log_id 106342/107076）: 過去の nostr_run 失敗の
/// tool_result 本文（エラーテキスト）に生 64hex（"Event not found: <64hex>"）が混じる。失敗本文は
/// 握り潰し防止で丸ごと残す（要約しない）が、表示は生識別子を短縮する。エラーの意味は残す。
#[test]
fn tool_result_failure_body_elides_raw_hex_but_keeps_error_text() {
    let conn = opencrab_db::init_memory().unwrap();
    let event_hex = format!("7be6255f{}", "a".repeat(56)); // 64hex（ダッシュ無し）
    let content = format!(
        r#"{{"success":false,"data":null,"error":"nostr_run failed: Event not found: {event_hex}"}}"#
    );
    let row = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: AGENT.into(),
        session_id: SESSION.into(),
        log_type: "tool_result".into(),
        content,
        speaker_id: Some(AGENT.into()),
        turn_number: None,
        metadata_json: Some(
            serde_json::json!({"tool_call_id": "tc-x", "tool_name": "nostr_run"}).to_string(),
        ),
        created_at: None,
    };
    opencrab_db::queries::insert_session_log(&conn, &row).unwrap();

    let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    let text = &assembled.text;
    // 生 64hex は消える（短縮形へ）。
    assert_eq!(count_uuids(text), 0, "UUID が残存: {text}");
    assert!(
        !text.contains(&event_hex),
        "失敗本文の生 64hex が残存: {text}"
    );
    assert!(
        text.contains("<id…>"),
        "生 hex が短縮形になっていない: {text}"
    );
    // エラーの意味は残す（握り潰さない）。
    assert!(
        text.contains("Event not found"),
        "エラーの意味が消えた: {text}"
    );
    // 検知器も沈黙（描画時に短縮済み）。
    assert!(
        crate::conversation::leaked_identifier_in_delta(text).is_none(),
        "検知器が生識別子を検出（描画器バグ）: {text}"
    );
}

/// row339 裁定の回帰固定: 発言本文（利用者・他者の自由記述）の識別子は**原文のまま** LLM プロンプト
/// （組立結果 = delta/live 描画経路）に現れる。QC 実弾で webhook 由来メッセージ本文の `runId:<UUID>`
/// が会話レンダリングで `<uuid…>` へ置換されて渡っていた回帰を潰す。本文の識別子改変は相手の発言の
/// 書き換え＝情報破壊。構造ラベル行の除去（`[… kind:N …]`）は不変。凍結 snapshot 経路（`strip_frozen_
/// snapshot`）は本テスト対象外（snapshot 無し = delta のみで組む）。
#[test]
fn speech_body_identifiers_reach_llm_prompt_verbatim() {
    let conn = opencrab_db::init_memory().unwrap();
    let run_id = "e059e80f-960c-45e3-b69c-ff493b133afc"; // webhook 由来の runId（ダッシュ付き UUID）
    let npub = format!("npub1{}", "q".repeat(58)); // 生 bech32
    let event_hex = format!("7be6255f{}", "a".repeat(56)); // 64hex（ダッシュ無し）
                                                           // webhook 転記の本文（構造ラベル行付き）。ラベル行は落とすが本文の識別子は原文のまま。
    let body =
        format!("runId: {run_id}\n作者 {npub} / event {event_hex}\n[inbound kind:1 メンション]");
    insert_speech(&conn, "webhook-user", &body);

    let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    let text = &assembled.text;

    // 本文の識別子は原文のまま（マスクしない）。
    assert!(
        text.contains(run_id),
        "本文の runId(UUID) が原文で残らない: {text}"
    );
    assert!(text.contains(&npub), "本文の npub が原文で残らない: {text}");
    assert!(
        text.contains(&event_hex),
        "本文の 64hex が原文で残らない: {text}"
    );
    // 置換プレースホルダを一切出さない。
    assert!(
        !text.contains("<uuid…>") && !text.contains("<npub…>") && !text.contains("<id…>"),
        "本文がマスクされている: {text}"
    );
    // 構造ラベル行は落とす（row294b・不変）。
    assert!(
        !text.contains("[inbound kind:"),
        "構造ラベル行が残存: {text}"
    );
}

/// row339 残穴（compaction 後の再マスク）の回帰固定: 発言本文に識別子を含む会話を **compaction で
/// 凍結 snapshot 化 → read 戻し** しても、本文の UUID/npub が原文のまま復元される（v2 マーカーで
/// read 時 full スクラブをスキップ）。この一周がないと snapshot 読み戻しで本文が再マスクされ、
/// 「本文原文」裁定が compaction 後に破れる。
#[test]
fn compaction_freeze_then_read_keeps_speech_body_identifiers_verbatim() {
    let conn = opencrab_db::init_memory().unwrap();
    let run_id = "e059e80f-960c-45e3-b69c-ff493b133afc"; // webhook 由来 runId（ダッシュ付き UUID）
    let npub = format!("npub1{}", "q".repeat(58)); // 生 bech32
                                                   // 高水位超えの履歴を積み、最後に識別子入りの発言（最新ユーザー＝RecentVerbatim で凍結後も残る）。
    seed_over_high(&conn);
    insert_speech(
        &conn,
        "webhook-user",
        &format!("runId: {run_id} / 作者 {npub}"),
    );

    // ターン終了 → compaction 発火 → snapshot 凍結（v2 マーカー付き）。
    let assembled = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    let mut gov = TurnGovernor::new(HIGH, LOW);
    let out = gov
        .finish_turn(
            &conn,
            SESSION,
            &assembled.items,
            assembled.through_log_id,
            &assembled.text,
        )
        .unwrap();
    assert!(
        out.fired,
        "前提: compaction が発火する（after={})",
        out.after_tokens
    );

    // 凍結後の read: snapshot（v2）から本文原文が復元される（再マスクなし）。
    let reread = assemble_from_snapshot(&conn, SESSION, AGENT).unwrap();
    let text = &reread.text;
    assert!(
        text.contains(run_id),
        "凍結→read で本文 UUID が再マスクされた: {text}"
    );
    assert!(
        text.contains(&npub),
        "凍結→read で本文 npub が再マスクされた: {text}"
    );
    assert!(
        !text.contains("<uuid…>") && !text.contains("<npub…>"),
        "凍結→read で本文がマスクされた: {text}"
    );
    // 世代マーカーは LLM テキストへ漏れない（read で除去）。
    assert!(
        !text.contains(crate::conversation::FROZEN_SNAPSHOT_V2_MARKER),
        "世代マーカーが会話テキストへ漏れた: {text}"
    );
}

/// 撤去の完全性。SQLite WAL の `wal_checkpoint` など別物は対象外。
#[test]
fn context_arrival_point_mechanism_is_gone() {
    let tool = format!("{}_{}", "update_context", "checkpoint");
    let marker = format!("{}_{}", "context", "checkpoint");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let mut hits = Vec::new();
    walk_for_needles(&root, &[&tool, &marker], &mut hits);
    assert!(
        hits.is_empty(),
        "到達点チェックポイント参照が残っている:\n{}",
        hits.join("\n")
    );
}

fn walk_for_needles(dir: &std::path::Path, needles: &[&str], hits: &mut Vec<String>) {
    const SKIP: &[&str] = &["target", ".git", "node_modules"];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIP.iter().any(|s| name == *s) {
            continue;
        }
        if path.is_dir() {
            walk_for_needles(&path, needles, hits);
            continue;
        }
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !matches!(ext, "rs" | "md" | "json" | "toml" | "ts" | "tsx" | "js") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for needle in needles {
            if text.contains(needle) {
                hits.push(format!("{}: {needle}", path.display()));
            }
        }
    }
}

