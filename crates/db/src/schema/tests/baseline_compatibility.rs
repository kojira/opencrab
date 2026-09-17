/// A. バージョン管理導入前の旧DBを模して、baseline が再適用され version 1 に
/// スタンプされることを検証する。
#[test]
fn baseline_reconciles_pre_versioning_db() {
    let conn = crate::init_memory().expect("init");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    // 旧DBを模す: version を 0 に戻し、baseline が再追加する列を落とす。
    conn.execute_batch("DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
    PRAGMA user_version = 0").unwrap();
    conn.execute_batch("ALTER TABLE skills DROP COLUMN archived")
        .unwrap();
    assert!(!column_exists(&conn, "skills", "archived").unwrap());

    // 再初期化で baseline + 番号付きマイグレーションが走り、列が復活し最新版にスタンプされる。
    initialize(&conn).expect("re-initialize");
    assert!(column_exists(&conn, "skills", "archived").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
}

/// #546: `idx_memory_sessions_session_type` は新規 DB（SCHEMA_SQL）にも既存 DB
/// （migration v39）にも届くこと。SCHEMA_SQL 側だけ／migration 側だけ、の食い違い
/// （#475 型の「既存 DB にだけ届かない」地雷）を両経路で固定する。
#[test]
fn session_type_index_reaches_new_and_existing_dbs() {
    fn has_session_type_index(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
             AND name='idx_memory_sessions_session_type'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    }

    // 新規 DB: SCHEMA_SQL 経路で index を持つ。
    let conn = crate::init_memory().expect("init");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
    assert!(has_session_type_index(&conn), "新規 DB に index が無い");

    // 既存 DB（v38・index 無し）を模す: index を落として版を 38 へ戻す。
    conn.execute_batch("DROP INDEX idx_memory_sessions_session_type; DROP INDEX IF EXISTS idx_gate_bindings_open_address_lookup;
    PRAGMA user_version = 38;")
        .unwrap();
    assert!(!has_session_type_index(&conn));

    // 再初期化で migration v39 が走り、index を復活し最新版へスタンプする。
    initialize(&conn).expect("re-initialize");
    assert!(
        has_session_type_index(&conn),
        "migration v39 が index を作っていない"
    );
    assert_eq!(schema_version(&conn).unwrap(), latest_version());
}

/// #489: co_agent 逆引き列（`agent_discord_config.bot_user_id` /
/// `agent_nostr_config.self_pubkey`）が新規 DB（SCHEMA_SQL）にも既存 DB（migration v40）にも
/// 届くこと。SCHEMA_SQL 側だけ／migration 側だけ、の食い違い（#475 型の「既存 DB にだけ
/// 届かない」地雷）を両経路で固定する。#546 と同型。
#[test]
fn initialize_is_idempotent_and_non_destructive() {
    let conn = crate::init_memory().expect("init");
    conn.execute_batch(
        "INSERT INTO agents (agent_id, name, persona_name) VALUES ('sentinel', 'n', 'p')",
    )
    .unwrap();

    initialize(&conn).expect("second initialize");
    assert_eq!(schema_version(&conn).unwrap(), latest_version());

    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM agents WHERE agent_id = 'sentinel'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "sentinel row must survive (baseline not re-run)");
}
