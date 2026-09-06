use super::super::*;
use super::support::seed_legacy_impressions;

/// v21: 人物像を agent スコープへ（#314）。
///
/// **重複が無い実データでは 1 行も失われない**こと。一意制約が
/// `(agent_id, target_id)` に付け替わり、同じ相手なら別セッションからでも
/// 同じ行を更新するようになること。
#[test]
fn impressions_agent_scope_migration_v21_preserves_rows() {
    let conn = crate::init_memory().expect("init");
    seed_legacy_impressions(
            &conn,
            "('i1', 'a1', 's-discord', 'u1', 'Alice', 'p1', '', '', '中立', '', 0, '2026-01-01', '2026-01-01'),
             ('i2', 'a1', 's-discord', 'u2', 'Bob',   'p2', '', '', '中立', '', 0, '2026-01-02', '2026-01-02'),
             ('i3', 'a1', 's-nostr',   'u3', 'Carol', 'p3', '', '', '中立', '', 0, '2026-01-03', '2026-01-03'),
             ('i4', 'a2', 's-discord', 'u1', 'Alice', 'p4', '', '', '中立', '', 0, '2026-01-04', '2026-01-04')",
        );

    initialize(&conn).expect("upgrade v20 -> v21");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 4 行すべて残る（重複が無いので統合は起きない）。
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM impressions ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(ids, vec!["i1", "i2", "i3", "i4"]);
    // 中身も session_id も動かない。
    let (session_id, personality): (String, String) = conn
        .query_row(
            "SELECT session_id, personality FROM impressions WHERE id = 'i3'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(session_id, "s-nostr");
    assert_eq!(personality, "p3");

    // 新しい一意制約: 同じ (agent_id, target_id) は別セッションでも 1 行。
    assert!(conn
            .execute(
                "INSERT INTO impressions (id, agent_id, session_id, target_id, target_name, created_at, updated_at) \
                 VALUES ('dup', 'a1', 's-other', 'u1', 'Alice', '2026-02-01', '2026-02-01')",
                [],
            )
            .is_err());
    // エージェントが違えば別の行（agent スコープは維持）。
    assert_eq!(
        crate::queries::get_impressions(&conn, "a1").unwrap().len(),
        3
    );
    assert_eq!(
        crate::queries::get_impressions(&conn, "a2").unwrap().len(),
        1
    );

    // 再実行しても冪等（再構築は走らない）。
    initialize(&conn).expect("idempotent");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM impressions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 4);
}

/// v21: 同じ相手が複数セッションに散っている（＝旧スキーマで分断していた）DB でも
/// 壊れない。統合方針は「`updated_at` が最新の行を残す・`created_at` は最古を継ぐ」。
#[test]
fn impressions_agent_scope_migration_v21_merges_duplicates() {
    let conn = crate::init_memory().expect("init");
    seed_legacy_impressions(
            &conn,
            "('old', 'a1', 's-discord', 'u1', 'Alice', 'old-note', '', '', '中立', '', 1, '2026-01-01', '2026-01-01'),
             ('new', 'a1', 's-nostr',   'u1', 'Alice2','new-note', '', '', '中立', '', 2, '2026-03-01', '2026-03-01'),
             ('keep','a1', 's-discord', 'u2', 'Bob',   'bob-note', '', '', '中立', '', 0, '2026-02-01', '2026-02-01')",
        );

    initialize(&conn).expect("upgrade v20 -> v21");

    let rows = crate::queries::get_impressions(&conn, "a1").unwrap();
    assert_eq!(rows.len(), 2, "u1 の 2 行が 1 行に統合される");

    let merged = crate::queries::get_impression(&conn, "a1", "u1")
        .unwrap()
        .expect("merged row");
    assert_eq!(merged.id, "new", "updated_at が新しい行が残る");
    assert_eq!(merged.personality, "new-note");
    assert_eq!(merged.target_name, "Alice2");
    assert_eq!(merged.session_id, "s-nostr");
    assert_eq!(merged.last_updated_turn, 2);
    // 「いつからの知り合いか」は統合対象の最古を引き継ぐ。
    let created_at: String = conn
        .query_row(
            "SELECT created_at FROM impressions WHERE target_id = 'u1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(created_at, "2026-01-01");

    // 重複していない相手は影響を受けない。
    let bob = crate::queries::get_impression(&conn, "a1", "u2")
        .unwrap()
        .expect("bob");
    assert_eq!(bob.id, "keep");
    assert_eq!(bob.personality, "bob-note");
}

/// v21: 旧制約の判定は `sqlite_master.sql` の文字列ではなく実際の索引の列を見る。
///
/// 同じ旧制約でも表記（空白・列の並べ方）は DB を作ったバイナリによって揺れる。
/// 表記が違っても再構築が走り、`upsert_impression` の `ON CONFLICT(agent_id,
/// target_id)` が通ること。
#[test]
fn impressions_agent_scope_migration_v21_detects_reformatted_legacy_schema() {
    let conn = crate::init_memory().expect("init");
    // 旧制約と等価だが `sql LIKE '%UNIQUE(agent_id, session_id, target_id)%'` には
    // 引っ掛からない表記（`UNIQUE (` と余分な空白）。
    conn.execute_batch(
        "DROP TABLE impressions;
             CREATE TABLE impressions (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                target_name TEXT NOT NULL,
                personality TEXT DEFAULT '',
                communication_style TEXT DEFAULT '',
                recent_behavior TEXT DEFAULT '',
                agreement TEXT DEFAULT '中立',
                notes TEXT DEFAULT '',
                last_updated_turn INTEGER DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE  (agent_id,session_id,target_id)
             );
             PRAGMA user_version = 20",
    )
    .unwrap();

    initialize(&conn).expect("upgrade v20 -> v21");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 新制約になっているので upsert（ON CONFLICT(agent_id, target_id)）が通る。
    let row = crate::queries::ImpressionRow {
        id: "i1".to_string(),
        agent_id: "a1".to_string(),
        session_id: "s-discord".to_string(),
        target_id: "u1".to_string(),
        target_name: "Alice".to_string(),
        personality: "p1".to_string(),
        communication_style: String::new(),
        recent_behavior: String::new(),
        agreement: "中立".to_string(),
        notes: String::new(),
        last_updated_turn: 0,
    };
    crate::queries::upsert_impression(&conn, &row).expect("upsert on new constraint");
    let mut again = row.clone();
    again.session_id = "s-nostr".to_string();
    again.personality = "p2".to_string();
    crate::queries::upsert_impression(&conn, &again).expect("upsert across sessions");
    assert_eq!(
        crate::queries::get_impressions(&conn, "a1").unwrap().len(),
        1,
        "別セッションからでも同じ 1 行を更新する"
    );
}
