use super::*;

/// 合成データ。実ログは公開リポジトリに置かない。
fn record(kind: &str, uuid: &str, ts: &str, content: Value) -> Value {
    serde_json::json!({
        "type": kind,
        "uuid": uuid,
        "sessionId": "11111111-2222-3333-4444-555555555555",
        "timestamp": ts,
        "cwd": "/work/proj",
        "gitBranch": "main",
        "message": { "role": kind, "content": content },
    })
}

fn plan(records: &[Value]) -> (Vec<PlannedRow>, ScanStats) {
    let mut stats = ScanStats::default();
    let mut rows = Vec::new();
    let mut idx = 1usize;
    for r in records {
        let raw = serde_json::to_string(r).unwrap();
        plan_record(r, raw.len(), &mut stats, &mut rows, &mut idx);
    }
    (rows, stats)
}

/// 本文 2 種は残り、ツール往復は 1 行も残らない（#413 の中核）。
#[test]
fn keeps_only_text_and_drops_tool_roundtrips() {
    let (rows, stats) = plan(&[
        record(
            "assistant",
            "u1",
            "2026-01-01T00:00:00.000Z",
            serde_json::json!([
                {"type": "text", "text": "答えはこうです"},
                {"type": "tool_use", "id": "tc1", "name": "read_file", "input": {"path": "a"}},
            ]),
        ),
        record(
            "user",
            "u2",
            "2026-01-01T00:00:01.000Z",
            serde_json::json!([
                {"type": "tool_result", "tool_use_id": "tc1", "content": "x".repeat(5000)},
            ]),
        ),
    ]);

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "答えはこうです");
    assert_eq!(rows[0].log_type, "speech");
    assert_eq!(rows[0].speaker, Speaker::Agent);
    assert!(stats.dropped.contains_key("assistant:tool_use"));
    assert!(stats.dropped.contains_key("user:tool_result"));
    // 落とした側のバイト数も測れている（何を捨てたかの実測が報告に要る）。
    assert!(stats.dropped["user:tool_result"].bytes > 5000);
}

/// 非対話の type は行ごと落ちる。**列挙していない未知の type も落ちる**
/// （Claude Code 側が type を増やしても勝手に混ざらない）。
#[test]
fn drops_every_non_conversational_type_including_unknown_ones() {
    let metas = [
        "file-history-snapshot",
        "queue-operation",
        "pr-link",
        "last-prompt",
        "mode",
        "permission-mode",
        "ai-title",
        "bridge-session",
        "attachment",
        "system",
        "some-future-type-nobody-has-seen",
    ];
    let records: Vec<Value> = metas
        .iter()
        .map(|t| serde_json::json!({"type": t, "sessionId": "s", "payload": "x"}))
        .collect();
    let (rows, stats) = plan(&records);
    assert!(rows.is_empty(), "非対話 type が取り込まれている: {rows:?}");
    for t in metas {
        assert!(stats.dropped.contains_key(&format!("meta:{t}")), "{t}");
    }
}

/// `user` の素の文字列 content（配列ではない）も人間の発言として拾う。
#[test]
fn plain_string_user_content_is_kept() {
    let (rows, _) = plan(&[record(
        "user",
        "u1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!("これをやって"),
    )]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "これをやって");
    assert_eq!(rows[0].speaker, Speaker::User);
}

/// `promptSource: "system"` の行のうち **task-notification だけ落とす**（道具が返した
/// 成果物ダンプ＝#393 と同型のノイズ）。他セッションからの連絡（agent-message 等）は
/// 「他者から連絡を受け判断を迫られた」対人的体験なので取り込み、`speaker_id` は
/// `system` のままにする。人間の発言は従来どおり残す（オーナーの UX 判断 #413）。
#[test]
fn task_notifications_are_dropped_but_other_system_rows_are_kept() {
    let human = record(
        "user",
        "u1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!("やあ"),
    );
    let mut task_note = record(
        "user",
        "u2",
        "2026-01-01T00:00:01.000Z",
        serde_json::json!("<task-notification>\n<status>completed</status>\n</task-notification>"),
    );
    task_note["promptSource"] = serde_json::json!("system");
    let mut cross_session = record(
        "user",
        "u3",
        "2026-01-01T00:00:02.000Z",
        serde_json::json!(
            "Another Claude session sent a message:\n\
             <agent-message from=\"general-purpose\">確認したい点があります</agent-message>"
        ),
    );
    cross_session["promptSource"] = serde_json::json!("system");

    let (rows, stats) = plan(&[human, task_note, cross_session]);

    // 入力 3 行のうち task-notification だけが落ち、2 行残る。
    assert_eq!(rows.len(), 2);
    // (c) 人間の発言は従来どおり。
    assert_eq!(rows[0].content, "やあ");
    assert_eq!(rows[0].speaker, Speaker::User);
    // (b) 他セッションからの連絡は取り込む。送信者は system 名義。
    assert!(rows[1].content.contains("<agent-message"));
    assert_eq!(rows[1].speaker, Speaker::System);
    assert_eq!(rows[1].speaker.speaker_id("agent-x"), SYSTEM_SPEAKER_ID);
    // (a) task-notification は落ちたことが集計に出る。
    assert_eq!(stats.dropped["user:task-notification"].count, 1);
    // 落ちたのは task-notification の 1 行だけ（system 名義の連絡は落ちていない）。
    assert_eq!(stats.dropped.values().map(|s| s.count).sum::<usize>(), 1);
}

/// thinking は**本文を 1 文字も生ログへ出さず**、印と退避先だけを残す。
#[test]
fn thinking_leaves_only_a_reference() {
    let secret = "ここが内心の本文である".repeat(50);
    let (rows, _) = plan(&[record(
        "assistant",
        "u1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!([{"type": "thinking", "thinking": secret, "signature": "sig"}]),
    )]);

    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.log_type, "inner_voice");
    assert!(
        !row.content.contains("ここが内心の本文"),
        "本文が生ログへ流れている: {}",
        row.content
    );
    assert!(row.content.starts_with("[think:th1]"), "{}", row.content);
    assert!(
        row.content.contains("claude_code/thinking/th1.txt"),
        "退避先が印に載っていない: {}",
        row.content
    );
    assert_eq!(row.thinking_body.as_deref(), Some(secret.as_str()));
    assert_eq!(
        row.thinking_rel_path().as_deref(),
        Some("claude_code/thinking/th1.txt")
    );
    // 印は本文よりずっと小さい（参照に置き換える意味がある）。
    assert!(row.content.len() < secret.len() / 10);
}

/// **実データのほとんどはこちら**: `thinking` が空文字列で `signature` だけがある形。
/// 印を作ると空ファイルを指す死んだ参照になるので、行にしない。
#[test]
fn thinking_without_a_body_makes_no_reference() {
    let (rows, stats) = plan(&[record(
        "assistant",
        "u1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!([{"type": "thinking", "thinking": "", "signature": "x".repeat(3000)}]),
    )]);
    assert!(rows.is_empty(), "空の thinking で印が立っている: {rows:?}");
    // 落としたことは集計に出る（「thinking が 15% ある」の実体が signature だと分かる）。
    assert!(stats.dropped.contains_key("assistant:thinking:empty"));
    assert!(stats.dropped["assistant:thinking:empty"].bytes > 3000);
}

/// 連番は与えられたカーソルから続く（再取り込みで既存の印と衝突しない）。
#[test]
fn thinking_ids_continue_from_the_cursor() {
    let mut stats = ScanStats::default();
    let mut rows = Vec::new();
    let mut idx = 42usize;
    for (i, uuid) in ["a", "b"].iter().enumerate() {
        let r = record(
            "assistant",
            uuid,
            &format!("2026-01-01T00:00:0{i}.000Z"),
            serde_json::json!([{"type": "thinking", "thinking": "考えた"}]),
        );
        plan_record(
            &r,
            serde_json::to_string(&r).unwrap().len(),
            &mut stats,
            &mut rows,
            &mut idx,
        );
    }
    assert_eq!(rows[0].thinking_id.as_deref(), Some("th42"));
    assert_eq!(rows[1].thinking_id.as_deref(), Some("th43"));
    assert_eq!(idx, 44);
}

#[test]
fn next_thinking_index_resumes_after_the_largest_existing_id() {
    let metas = [
        r#"{"source":"claude_code","uuid":"a","block":0,"think_id":"th7"}"#,
        r#"{"source":"claude_code","uuid":"b","block":0,"think_id":"th12"}"#,
        r#"{"source":"claude_code","uuid":"c","block":0}"#,
        r#"{"source":"discord"}"#,
    ];
    assert_eq!(next_thinking_index(metas), 13);
    assert_eq!(next_thinking_index(std::iter::empty()), 1);
}

/// `timestamp` は DB の他の行と同じ表記へ揃える（文字列比較の順序を壊さない）。
#[test]
fn timestamps_are_normalized_to_the_db_representation() {
    let (rows, _) = plan(&[record(
        "user",
        "u1",
        "2026-06-29T03:38:11.126Z",
        serde_json::json!("hi"),
    )]);
    assert_eq!(rows[0].created_at, "2026-06-29T03:38:11.126+00:00");
    // 素の `Z` 表記のままだと同じ秒で常に後ろへ回る（回避できていることの根拠）。
    assert!("2026-06-29T03:38:11.126Z" > "2026-06-29T03:38:11.126456+00:00");
    assert!(rows[0].created_at.as_str() < "2026-06-29T03:38:11.126456+00:00");
}

/// 時刻の無い対話行は取り込まない（id 昇順＝時刻昇順を崩さない）。
#[test]
fn rows_without_a_timestamp_are_dropped() {
    let mut r = record(
        "user",
        "u1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!("hi"),
    );
    r["timestamp"] = serde_json::json!("not a timestamp");
    let (rows, stats) = plan(&[r]);
    assert!(rows.is_empty());
    assert_eq!(stats.undatable_rows, 1);
}

/// 空本文は行にしない。
#[test]
fn empty_text_is_not_a_row() {
    let (rows, _) = plan(&[record(
        "assistant",
        "u1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!([{"type": "text", "text": "   \n"}]),
    )]);
    assert!(rows.is_empty());
}

/// 同じファイルを 2 回取り込んでも増えない。
#[test]
fn re_import_adds_nothing() {
    let recs = [
        record(
            "user",
            "u1",
            "2026-01-01T00:00:00.000Z",
            serde_json::json!("やあ"),
        ),
        record(
            "assistant",
            "u2",
            "2026-01-01T00:00:01.000Z",
            serde_json::json!([{"type": "text", "text": "はい"}]),
        ),
    ];
    let (rows, _) = plan(&recs);
    assert_eq!(rows.len(), 2);

    // 1 回目の結果を「既存行」に見立てる。
    let stored: Vec<(String, String)> = rows
        .iter()
        .map(|r| (r.session_id.clone(), r.metadata_json()))
        .collect();
    let keys =
        imported_keys_from_metadata(stored.iter().map(|(s, m)| (s.as_str(), Some(m.as_str()))));

    let (again, _) = plan(&recs);
    assert!(filter_already_imported(again, &keys).is_empty());
}

/// 追記された行だけが 2 回目に入る（走行中セッションの取り込み）。
#[test]
fn re_import_picks_up_only_appended_records() {
    let first = [record(
        "user",
        "u1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!("やあ"),
    )];
    let (rows, _) = plan(&first);
    let stored: Vec<(String, String)> = rows
        .iter()
        .map(|r| (r.session_id.clone(), r.metadata_json()))
        .collect();
    let keys =
        imported_keys_from_metadata(stored.iter().map(|(s, m)| (s.as_str(), Some(m.as_str()))));

    let second = [
        first[0].clone(),
        record(
            "assistant",
            "u2",
            "2026-01-01T00:00:01.000Z",
            serde_json::json!([{"type": "text", "text": "あとから追記"}]),
        ),
    ];
    let (again, _) = plan(&second);
    let remaining = filter_already_imported(again, &keys);
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].content, "あとから追記");
}

/// 他の由来（discord 等）の行は重複防止の鍵に混ざらない。
#[test]
fn other_sources_are_not_treated_as_imported() {
    let keys = imported_keys_from_metadata([
        ("sess-d", Some(r#"{"source":"discord","user_name":"x"}"#)),
        (
            "cc-1",
            Some(r#"{"source":"claude_code","uuid":"u1","block":0}"#),
        ),
        ("cc-1", None),
    ]);
    assert_eq!(keys.len(), 1);
    assert!(keys.contains(&("cc-1".to_string(), "u1".to_string(), 0)));
}

/// 同じレコードの複数ブロックは別行として区別される（鍵にブロック位置が要る）。
#[test]
fn blocks_of_one_record_are_distinct_rows() {
    let (rows, _) = plan(&[record(
        "assistant",
        "u1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!([
            {"type": "text", "text": "まず"},
            {"type": "tool_use", "id": "t", "name": "n", "input": {}},
            {"type": "text", "text": "つぎに"},
        ]),
    )]);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].block_index, 0);
    assert_eq!(rows[1].block_index, 2);
    assert_ne!(rows[0].dedup_key(), rows[1].dedup_key());
}

/// プロジェクト全体で時刻昇順に並ぶ（セッションが並行しても id 順＝時刻順）。
#[test]
fn project_rows_are_ordered_by_time_across_sessions() {
    let dir = tempfile::TempDir::new().unwrap();
    let mk = |session: &str, ts: &[&str]| {
        let lines: Vec<String> = ts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut r = record("user", &format!("{session}-{i}"), t, serde_json::json!("x"));
                r["sessionId"] = serde_json::json!(session);
                serde_json::to_string(&r).unwrap()
            })
            .collect();
        std::fs::write(
            dir.path().join(format!("{session}.jsonl")),
            lines.join("\n"),
        )
        .unwrap();
    };
    mk(
        "aaa",
        &["2026-01-01T00:00:00.000Z", "2026-01-01T00:00:04.000Z"],
    );
    mk(
        "bbb",
        &["2026-01-01T00:00:02.000Z", "2026-01-01T00:00:03.000Z"],
    );

    let scan = scan_project_dir(dir.path(), 1).unwrap();
    let order: Vec<&str> = scan.rows.iter().map(|r| r.created_at.as_str()).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted, "時刻順に並んでいない");
    assert_eq!(scan.rows.len(), 4);
    assert_eq!(scan.stats.files, 2);
    // セッションはまたぐが session_id は元のセッションを保つ。
    assert_eq!(scan.rows[0].session_id, "cc-aaa");
    assert_eq!(scan.rows[1].session_id, "cc-bbb");
}

/// 走行中のセッション（末尾が書きかけ）でも、読める行は全部取り込む。
#[test]
fn a_truncated_last_line_does_not_lose_the_rest() {
    let dir = tempfile::TempDir::new().unwrap();
    let good = serde_json::to_string(&record(
        "user",
        "u1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!("よめる"),
    ))
    .unwrap();
    std::fs::write(
        dir.path().join("s.jsonl"),
        format!("{good}\n{{\"type\":\"user\",\"mess"),
    )
    .unwrap();

    let scan = scan_project_dir(dir.path(), 1).unwrap();
    assert_eq!(scan.rows.len(), 1);
    assert_eq!(scan.stats.unparsable_lines, 1);
}

/// `.jsonl` 以外や、サブディレクトリ配下のファイルは走査しない。実レイアウトでは
/// `<project>/<uuid>/subagents/agent-*.jsonl` にサブエージェント（サイドチェーン）の
/// セッションログが入るが、[`scan_project_dir`] は非再帰なので読まれない。サイドチェーンは
/// さらに [`plan_record`] で `isSidechain` を見て**明示的にも**落とす（この非再帰の
/// 偶然に頼らない）。
#[test]
fn only_jsonl_files_are_scanned() {
    let dir = tempfile::TempDir::new().unwrap();
    // サブエージェントのログは `<uuid>/subagents/` 配下（サブディレクトリ）に置かれる。
    let subagents = dir
        .path()
        .join("11111111-2222-3333-4444-555555555555")
        .join("subagents");
    std::fs::create_dir_all(&subagents).unwrap();
    let sidechain_log = serde_json::to_string(&record(
        "assistant",
        "sa1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!([{"type": "text", "text": "サブエージェントの本文"}]),
    ))
    .unwrap();
    std::fs::write(subagents.join("agent-abc.jsonl"), sidechain_log).unwrap();
    std::fs::write(dir.path().join("notes.md"), "not a session").unwrap();

    let scan = scan_project_dir(dir.path(), 1).unwrap();
    // サブディレクトリ配下の `.jsonl` も `.md` も読まない。
    assert_eq!(scan.stats.files, 0);
    assert!(scan.rows.is_empty());
}

/// `isSidechain: true`（サブエージェント＝道具の動き）のレコードは type を問わず落ち、
/// dropped に載る。既存の取り込み対象（本人の発話）は従来どおり残る。
#[test]
fn sidechain_records_are_dropped() {
    let normal = record(
        "assistant",
        "u1",
        "2026-01-01T00:00:00.000Z",
        serde_json::json!([{"type": "text", "text": "本人の発話"}]),
    );
    let mut side = record(
        "assistant",
        "u2",
        "2026-01-01T00:00:01.000Z",
        serde_json::json!([{"type": "text", "text": "サブエージェントの発話"}]),
    );
    side["isSidechain"] = serde_json::json!(true);

    let (rows, stats) = plan(&[normal, side]);

    // 入力 2 行のうちサイドチェーンだけが落ち、本人の行だけが残る。
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "本人の発話");
    // 落としたことは集計に出る（type 別のキーで載る）。
    assert_eq!(stats.dropped["assistant:sidechain"].count, 1);
}
