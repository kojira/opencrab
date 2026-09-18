/// v20 起点の一気通貫（v20→v21→v22→v23）。稼働中の本番 DB は v22 なので実運用の
/// 経路は v22→v23 だが、新規環境や古い DB からの復元では v20 から連鎖する。この道で
/// (1) memory_index の時系列ツリーが 1 件も失われず CHECK が広がること、(2) 途中の
/// v21（impressions を agent スコープへ）と v22（owner_pubkey 追加）も併せて適用され
/// 最終版へ到達することを固定する（従来は user_version=22 を手で置いた単独移行のみ）。
#[test]
fn task_ledger_migration_upgrades_v1_db() {
    let conn = crate::init_memory().expect("init");
    // v1 相当の既存DBを模す: タスク台帳を落として version 1 に戻す。
    conn.execute_batch("DROP TABLE task_progress; DROP TABLE task_ledger; DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
    PRAGMA user_version = 1")
        .unwrap();
    assert!(!table_exists(&conn, "task_ledger").unwrap());

    initialize(&conn).expect("upgrade v1 -> latest");
    assert!(table_exists(&conn, "task_ledger").unwrap());
    assert!(table_exists(&conn, "task_progress").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
}

/// v3: display_name 列の付与。v2 相当 DB（列なし）からのアップグレードと、
/// 新規 DB（SCHEMA_SQL 由来で列あり）での冪等性の両方を確認する。
#[test]
fn v17_never_destroys_populated_trusted_users() {
    let conn = Connection::open_in_memory().expect("open");
    // 新表（最終 shape）にデータを 1 件、旧表にも別データを 1 件置いた並存状態を作る。
    conn.execute_batch(
            "CREATE TABLE trusted_users (
               id TEXT PRIMARY KEY,
               user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               display_name TEXT NOT NULL DEFAULT '',
               platform TEXT NOT NULL DEFAULT 'discord',
               UNIQUE (user_id, agent_id)
             );
             INSERT INTO trusted_users (id, user_id, agent_id, permission, created_by, created_at, display_name, platform)
               VALUES ('new-1', '100', 'a1', 'co-agent', 'owner', '2026-05-01', 'Keep Me', 'web');
             CREATE TABLE trusted_discord_users (
               id TEXT PRIMARY KEY,
               discord_user_id TEXT NOT NULL,
               agent_id TEXT NOT NULL,
               permission TEXT NOT NULL DEFAULT 'user',
               created_by TEXT NOT NULL DEFAULT 'owner',
               created_at TEXT NOT NULL,
               display_name TEXT NOT NULL DEFAULT '',
               platform TEXT NOT NULL DEFAULT 'discord',
               UNIQUE (discord_user_id, agent_id)
             );
             INSERT INTO trusted_discord_users (id, discord_user_id, agent_id, permission, created_by, created_at, display_name, platform)
               VALUES ('old-1', '42', 'a1', 'user', 'owner', '2026-01-01', 'Stale', 'discord');",
        )
        .unwrap();

    // v17 の up を直接適用（並存状態に対する分岐だけを検証する）。
    let v17 = MIGRATIONS
        .iter()
        .find(|m| m.version == 17)
        .expect("v17 migration exists");
    (v17.up)(&conn).expect("v17 up");

    // 新表のデータはそのまま（DROP されていない・置き換わっていない）。
    let (n, display): (i64, String) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM trusted_users), \
                        (SELECT display_name FROM trusted_users WHERE id = 'new-1')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(n, 1, "既存データのある trusted_users は 1 行のまま");
    assert_eq!(
        display, "Keep Me",
        "既存行が旧表のデータで上書きされないこと"
    );
    // 旧表は触られず残る（実データを勝手に消さない）。ここでの並存は通常経路では起きないが、
    // 起きても「新表のデータを守る」方を優先する。
    assert!(
        table_exists(&conn, "trusted_discord_users").unwrap(),
        "新表にデータがある場合、旧表は破棄されない"
    );

    // 冪等: もう一度流しても新表のデータは不変。
    (v17.up)(&conn).expect("v17 up idempotent");
    let n2: i64 = conn
        .query_row("SELECT COUNT(*) FROM trusted_users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n2, 1);
}

/// v18: `trusted_users.permission` の表記統一（#234）。
///
/// **移行前後で権限の判定結果が変わらない**こと（同じ人が同じ権限のまま、表記だけ
/// ケバブケースになる）。行は 1 件も増減しない。旧い綴りのうち**判定が完全一致で
/// 見ていたもの（`co_agent`）だけ**を移し、それ以外は触らない（権限を増やさない）。
#[test]
fn permission_spelling_migration_rewrites_rows_without_changing_who_is_a_co_agent() {
    use crate::queries::TrustedUserPermission;

    let conn = crate::init_memory().expect("init");
    // v17 相当の既存 DB を模す: 旧表記の行を含めて 4 件入れ、version 17 へ戻す。
    conn.execute_batch(
        "DELETE FROM trusted_users;
             INSERT INTO trusted_users
               (id, user_id, agent_id, permission, created_by, created_at, display_name, platform)
               VALUES ('r1', '42', 'a1', 'co_agent', 'owner', '2026-01-01', 'Crab B', 'discord'),
                      ('r2', '43', 'a1', 'user',     'owner', '2026-01-02', '',       'discord'),
                      ('r3', '44', 'a1', 'owner',    'owner', '2026-01-03', '',       'discord'),
                      ('r4', '45', 'a1', 'coagent',  'owner', '2026-01-04', 'Typo',   'discord');
             DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
             PRAGMA user_version = 17",
    )
    .unwrap();

    // 移行前の判定（旧い読み出し = permission == 'co_agent' の完全一致）。
    let judged_before: Vec<(String, bool)> = conn
        .prepare("SELECT id, permission FROM trusted_users ORDER BY id")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)? == "co_agent"))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    initialize(&conn).expect("upgrade v17 -> v18");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 行は増減しない。
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM trusted_users", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 4);

    // 移行後の判定（新しい読み出し = 列挙型）。移行前と一致すること。
    let judged_after: Vec<(String, bool)> = conn
        .prepare("SELECT id, permission FROM trusted_users ORDER BY id")
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                TrustedUserPermission::from_db_str(&r.get::<_, String>(1)?)
                    == TrustedUserPermission::CoAgent,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(judged_before, judged_after);
    assert_eq!(
        judged_after,
        vec![
            ("r1".to_string(), true),
            ("r2".to_string(), false),
            ("r3".to_string(), false),
            ("r4".to_string(), false),
        ]
    );

    // 表記は移り、触らない行はそのまま（`coagent` は判定が拾っていなかったので拾わない）。
    let spellings: Vec<String> = conn
        .prepare("SELECT permission FROM trusted_users ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(spellings, vec!["co-agent", "user", "owner", "coagent"]);

    // 再実行しても冪等（2 回目は 0 行更新）。
    initialize(&conn).expect("idempotent");
    let after: Vec<String> = conn
        .prepare("SELECT permission FROM trusted_users ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(after, spellings);
}
