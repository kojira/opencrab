use super::*;

// ============================================
// スリープ整理ラン（#313 段階3 / #361）: マーカー + worklist クエリ
// ============================================

#[test]
fn organize_marker_get_set_roundtrip_and_default_none() {
    let conn = setup();
    // config 行が無ければ None（get_memory_index_config の非永続デフォルトに引きずられない）。
    assert_eq!(get_last_organize_at(&conn, "a1").unwrap(), None);
    // set は行を作る（UPSERT）。
    set_last_organize_at(&conn, "a1", "2026-08-03T00:00:00Z").unwrap();
    assert_eq!(
        get_last_organize_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-03T00:00:00Z")
    );
    // 上書きは last_organize_at のみを更新し、他エージェントに漏れない。
    set_last_organize_at(&conn, "a1", "2026-08-04T00:00:00Z").unwrap();
    assert_eq!(
        get_last_organize_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-04T00:00:00Z")
    );
    assert_eq!(get_last_organize_at(&conn, "a2").unwrap(), None);
}

#[test]
fn organize_marker_does_not_disturb_skill_consolidation_marker() {
    let conn = setup();
    // 既存の skill 棚卸しマーカーが立っている状態で organize マーカーを刻んでも、
    // 隣の列は消えない（同じ config 行を共有するため）。
    set_last_skill_consolidation_at(&conn, "a1", "2026-07-01T00:00:00Z").unwrap();
    set_last_organize_at(&conn, "a1", "2026-08-03T00:00:00Z").unwrap();
    assert_eq!(
        get_last_skill_consolidation_at(&conn, "a1")
            .unwrap()
            .as_deref(),
        Some("2026-07-01T00:00:00Z")
    );
    assert_eq!(
        get_last_organize_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-03T00:00:00Z")
    );
}

#[test]
fn organize_worklist_respects_since_snapshot_limit_and_order() {
    let conn = setup();
    // スナップショット内（end_log_id <= 100）で、マーカー(2026-08-02)以降の topic を古い順に。
    seed_topic(
        &conn,
        "a1",
        "old",
        "t1",
        "2026-08-01T00:00:00Z",
        Some(10),
        "session_log",
    ); // マーカー前 → 除外
    seed_topic(
        &conn,
        "a1",
        "n1",
        "t2",
        "2026-08-03T00:00:00Z",
        Some(50),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "n2",
        "t3",
        "2026-08-04T00:00:00Z",
        Some(80),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "future",
        "t4",
        "2026-08-05T00:00:00Z",
        Some(200),
        "session_log",
    ); // snapshot 超過 → 除外
       // topic 以外・他エージェント・category は対象外。
    seed_topic(
        &conn,
        "a1",
        "cat",
        "c1",
        "2026-08-03T00:00:00Z",
        Some(60),
        "category",
    );
    seed_topic(
        &conn,
        "a2",
        "other",
        "o1",
        "2026-08-03T00:00:00Z",
        Some(60),
        "session_log",
    );

    let since = Some(("2026-08-02T00:00:00Z", ""));
    // 件数ゲート（下限判定用）。
    assert_eq!(count_organize_topics(&conn, "a1", since, 100).unwrap(), 2);
    // worklist は (created_at, id) 昇順で n1, n2。
    let wl = list_organize_topics(&conn, "a1", since, 100, 50).unwrap();
    let ids: Vec<&str> = wl.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["n1", "n2"]);

    // since=None なら下端制約なし（マーカー前の old も入る。future は依然 snapshot 超過で除外）。
    assert_eq!(count_organize_topics(&conn, "a1", None, 100).unwrap(), 3);
}

#[test]
fn organize_worklist_limit_leaves_remainder_for_next_time() {
    let conn = setup();
    for i in 1..=5 {
        // created_at を昇順に振る（08-03T00:0i）。
        let ts = format!("2026-08-03T00:0{i}:00Z");
        seed_topic(
            &conn,
            "a1",
            &format!("n{i}"),
            &format!("t{i}"),
            &ts,
            Some(10 + i),
            "session_log",
        );
    }
    let since = Some(("2026-08-02T00:00:00Z", ""));
    // 下限判定は全 5 件を数える。
    assert_eq!(count_organize_topics(&conn, "a1", since, 100).unwrap(), 5);
    // N=3 で切ると (created_at, id) 昇順 3 件。残りの n4/n5 はより新しいので、
    // 末尾の (created_at, id) カーソルを刻めば次回に拾える（前進のみ / 残りは次回）。
    let wl = list_organize_topics(&conn, "a1", since, 100, 3).unwrap();
    let ids: Vec<&str> = wl.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["n1", "n2", "n3"]);
    let last = wl.last().unwrap();
    let cursor = (last.created_at.as_str(), last.id.as_str());

    // 次回: カーソルを進めると残り 2 件だけが対象（重複しない）。
    let next = list_organize_topics(&conn, "a1", Some(cursor), 100, 3).unwrap();
    let next_ids: Vec<&str> = next.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(next_ids, vec!["n4", "n5"]);
}

#[test]
fn organize_worklist_includes_null_end_log_id() {
    let conn = setup();
    // end_log_id NULL（索引済みとみなす）は snapshot 内として拾う。
    seed_topic(
        &conn,
        "a1",
        "n1",
        "t1",
        "2026-08-03T00:00:00Z",
        None,
        "session_log",
    );
    assert_eq!(
        count_organize_topics(&conn, "a1", Some(("2026-08-02T00:00:00Z", "")), 5).unwrap(),
        1
    );
}

/// 回帰（blocker / PR #364 レビュー）: 索引ビルドは 1 パスの全 topic に**同一 created_at**
/// を刻む（`index_builder.rs`）。新規が N を超え、切り口が同着群の内側に落ちても、
/// `(created_at, id)` カーソルなら残余を次回に引き継いで取りこぼさないこと。
#[test]
fn organize_worklist_same_created_at_group_not_dropped_across_runs() {
    let conn = setup();
    let ts = "2026-08-03T00:00:00Z";
    // 同着 created_at の 51 件（id は昇順で相異）。本番の topic id 形に寄せる。
    for i in 0..51 {
        seed_topic(
            &conn,
            "a1",
            &format!("topic-a1-s-{i:03}"),
            &format!("t{i:03}"),
            ts,
            Some(10),
            "session_log",
        );
    }
    let since = Some(("2026-08-02T00:00:00Z", ""));
    // run1: N=50。
    let wl1 = list_organize_topics(&conn, "a1", since, 100, 50).unwrap();
    assert_eq!(wl1.len(), 50);
    // マーカー前進 = 提示した末尾の (created_at, id) カーソル。
    let last = wl1.last().unwrap();
    let cursor = (last.created_at.as_str(), last.id.as_str());
    // run2: 残り 1 件が次回に拾える（同着でも取りこぼさない）。
    let wl2 = list_organize_topics(&conn, "a1", Some(cursor), 100, 50).unwrap();
    assert_eq!(wl2.len(), 1, "同着 created_at 群の残余が取りこぼされている");
    // run1 と run2 は重複しない（カーソルより後の 1 件だけ）。
    assert!(
        wl1.iter().all(|t| t.id != wl2[0].id),
        "run1 と run2 の topic が重複している"
    );
    // count も同じカーソルで残余 1 を返す（ゲートと worklist の整合）。
    assert_eq!(
        count_organize_topics(&conn, "a1", Some(cursor), 100).unwrap(),
        1
    );
}

// ============================================
// スリープ整理ラン（#313 段階3b / #365）: 過去分の遡り消化マーカー + 降順 worklist
// ============================================

#[test]
fn organize_backlog_cursor_get_set_roundtrip_and_default_none() {
    let conn = setup();
    // 行が無ければ None。
    assert_eq!(get_organize_backlog_cursor(&conn, "a1").unwrap(), None);
    set_organize_backlog_cursor(&conn, "a1", "2026-08-03T00:00:00Z|old1").unwrap();
    assert_eq!(
        get_organize_backlog_cursor(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-03T00:00:00Z|old1")
    );
    // 上書きは当該列のみ。他エージェントに漏れない。
    set_organize_backlog_cursor(&conn, "a1", "2026-08-01T00:00:00Z|old9").unwrap();
    assert_eq!(
        get_organize_backlog_cursor(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-01T00:00:00Z|old9")
    );
    assert_eq!(get_organize_backlog_cursor(&conn, "a2").unwrap(), None);
}

#[test]
fn organize_last_run_at_get_set_roundtrip_and_default_none() {
    let conn = setup();
    assert_eq!(get_organize_last_run_at(&conn, "a1").unwrap(), None);
    set_organize_last_run_at(&conn, "a1", "2026-08-03T00:00:00Z").unwrap();
    assert_eq!(
        get_organize_last_run_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-03T00:00:00Z")
    );
    set_organize_last_run_at(&conn, "a1", "2026-08-04T00:00:00Z").unwrap();
    assert_eq!(
        get_organize_last_run_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-04T00:00:00Z")
    );
    assert_eq!(get_organize_last_run_at(&conn, "a2").unwrap(), None);
}

#[test]
fn organize_markers_three_axes_do_not_disturb_each_other() {
    let conn = setup();
    // 3 マーカー（新規位置 / 遡り位置 / throttle 刻時）+ skill 棚卸しが同じ config 行を
    // 共有しても互いに消えない。
    set_last_skill_consolidation_at(&conn, "a1", "2026-07-01T00:00:00Z").unwrap();
    set_last_organize_at(&conn, "a1", "2026-08-04T00:00:00Z|n5").unwrap();
    set_organize_backlog_cursor(&conn, "a1", "2026-06-01T00:00:00Z|old3").unwrap();
    set_organize_last_run_at(&conn, "a1", "2026-08-05T00:00:00Z").unwrap();
    assert_eq!(
        get_last_skill_consolidation_at(&conn, "a1")
            .unwrap()
            .as_deref(),
        Some("2026-07-01T00:00:00Z")
    );
    assert_eq!(
        get_last_organize_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-04T00:00:00Z|n5")
    );
    assert_eq!(
        get_organize_backlog_cursor(&conn, "a1").unwrap().as_deref(),
        Some("2026-06-01T00:00:00Z|old3")
    );
    assert_eq!(
        get_organize_last_run_at(&conn, "a1").unwrap().as_deref(),
        Some("2026-08-05T00:00:00Z")
    );
}

#[test]
fn organize_backlog_respects_boundary_snapshot_and_desc_order() {
    let conn = setup();
    // 境界（遡りカーソル）= 2026-08-02。これより古い topic だけが過去分。
    seed_topic(
        &conn,
        "a1",
        "b1",
        "t1",
        "2026-08-01T00:00:00Z",
        Some(30),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "b2",
        "t2",
        "2026-07-15T00:00:00Z",
        Some(20),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "b3",
        "t3",
        "2026-07-01T00:00:00Z",
        Some(10),
        "session_log",
    );
    // 境界より新しい（=新規側の領域）→ 過去分に入らない。
    seed_topic(
        &conn,
        "a1",
        "recent",
        "t4",
        "2026-08-05T00:00:00Z",
        Some(40),
        "session_log",
    );
    // snapshot 超過（end_log_id > 100）→ 除外。
    seed_topic(
        &conn,
        "a1",
        "beyond",
        "t5",
        "2026-07-10T00:00:00Z",
        Some(200),
        "session_log",
    );
    // 別エージェント・category → 対象外。
    seed_topic(
        &conn,
        "a2",
        "o1",
        "o",
        "2026-07-05T00:00:00Z",
        Some(20),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "cat",
        "c1",
        "2026-07-05T00:00:00Z",
        Some(20),
        "category",
    );

    let before = ("2026-08-02T00:00:00Z", "");
    // 残数（監査・先頭到達判定）= b1,b2,b3 の 3 件。
    assert_eq!(
        count_organize_backlog_topics(&conn, "a1", before, 100).unwrap(),
        3
    );
    // worklist は created_at **降順**（新しい過去分から遡る）: b1(08-01) → b2(07-15) → b3(07-01)。
    let wl = list_organize_backlog_topics(&conn, "a1", before, 100, 50).unwrap();
    let ids: Vec<&str> = wl.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["b1", "b2", "b3"]);
}

/// 回帰（#364 blocker と同型・**降順側**）: 索引ビルドは 1 パスの全 topic に同一 created_at を
/// 刻む。遡りが N を超え、切り口が同着群の内側に落ちても、`(created_at, id)` カーソル（降順）
/// なら残余を次回に引き継いで取りこぼさないこと。
#[test]
fn organize_backlog_same_created_at_group_not_dropped_descending() {
    let conn = setup();
    let ts = "2026-07-01T00:00:00Z"; // 境界より古い同着 created_at
    for i in 0..51 {
        seed_topic(
            &conn,
            "a1",
            &format!("topic-a1-s-{i:03}"),
            &format!("t{i:03}"),
            ts,
            Some(10),
            "session_log",
        );
    }
    // 境界は同着群より新しい任意時刻。id="" 付きなので created_at < 境界 の全件が対象。
    let before = ("2026-08-01T00:00:00Z", "");
    // run1: N=50（降順 = id 降順で上位 50 = 050..001）。
    let wl1 = list_organize_backlog_topics(&conn, "a1", before, 100, 50).unwrap();
    assert_eq!(wl1.len(), 50);
    // 遡りマーカー = 提示した中で最も古い（降順の末尾）の (created_at, id)。
    let oldest = wl1.last().unwrap();
    let cursor = (oldest.created_at.as_str(), oldest.id.as_str());
    // run2: 残り 1 件（同着でも取りこぼさない）。
    let wl2 = list_organize_backlog_topics(&conn, "a1", cursor, 100, 50).unwrap();
    assert_eq!(
        wl2.len(),
        1,
        "同着 created_at 群の残余が降順側で取りこぼされている"
    );
    assert!(
        wl1.iter().all(|t| t.id != wl2[0].id),
        "run1 と run2 の topic が重複している"
    );
    // count も同じカーソルで残余 1（ゲートと worklist の整合）。
    assert_eq!(
        count_organize_backlog_topics(&conn, "a1", cursor, 100).unwrap(),
        1
    );
}

#[test]
fn organize_backlog_reaches_head_returns_empty() {
    let conn = setup();
    seed_topic(
        &conn,
        "a1",
        "b1",
        "t1",
        "2026-07-01T00:00:00Z",
        Some(10),
        "session_log",
    );
    seed_topic(
        &conn,
        "a1",
        "b2",
        "t2",
        "2026-06-01T00:00:00Z",
        Some(11),
        "session_log",
    );
    // カーソルを最古（b2）ちょうどに置く: b2 は `id < ""`不成立で除外、b1 は created_at>cursor で除外。
    let before = ("2026-06-01T00:00:00Z", "");
    assert_eq!(
        count_organize_backlog_topics(&conn, "a1", before, 100).unwrap(),
        0,
        "先頭到達で残数 0"
    );
    let wl = list_organize_backlog_topics(&conn, "a1", before, 100, 50).unwrap();
    assert!(wl.is_empty(), "先頭到達で 0 件（無限に走らない）");
}
