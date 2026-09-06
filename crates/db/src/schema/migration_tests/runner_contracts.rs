use super::super::*;
use rusqlite::Connection;
fn create_marker(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE TABLE test_marker (id INTEGER)")
}

/// C. 番号付きマイグレーションのランナー: 未適用の version 2 を適用し、
/// 再実行では no-op になる（version gate）。
#[test]
fn run_migrations_applies_and_then_skips() {
    let conn = crate::init_memory().expect("init");
    // 実 MIGRATIONS 適用済み（= 最新版）なので、fake v2 が未適用となる状態に戻す。
    conn.execute_batch("PRAGMA user_version = 1").unwrap();
    let fake = &[Migration {
        version: 2,
        description: "add test_marker",
        up: create_marker,
    }];

    run_migrations(&conn, fake).expect("apply v2");
    assert!(table_exists(&conn, "test_marker").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), 2);

    // 再実行は no-op（既に version 2）。
    run_migrations(&conn, fake).expect("re-run no-op");
    assert_eq!(schema_version(&conn).unwrap(), 2);
}

fn fail_after_marker(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE TABLE test_marker (id INTEGER)")?;
    Err(rusqlite::Error::InvalidQuery)
}

/// D. マイグレーション失敗時は、その up の変更と version スタンプが
/// トランザクションごとロールバックされる。
#[test]
fn failed_migration_rolls_back_and_leaves_version() {
    let conn = crate::init_memory().expect("init");
    // 実 MIGRATIONS 適用済みなので、fake v2 が適用対象となる状態に戻す。
    conn.execute_batch("PRAGMA user_version = 1").unwrap();
    let fake = &[Migration {
        version: 2,
        description: "fails",
        up: fail_after_marker,
    }];

    let result = run_migrations(&conn, fake);
    assert!(result.is_err());
    assert!(!table_exists(&conn, "test_marker").unwrap());
    assert_eq!(schema_version(&conn).unwrap(), BASELINE_VERSION);
}

/// F. ダウングレードガード: DB が既知の最新版より新しい場合はエラーにする。
#[test]
fn downgrade_is_rejected() {
    let conn = crate::init_memory().expect("init");
    conn.execute_batch("PRAGMA user_version = 999").unwrap();
    let fake = &[Migration {
        version: 2,
        description: "v2",
        up: create_marker,
    }];
    let result = run_migrations(&conn, fake);
    assert!(result.is_err(), "newer-than-supported DB must be rejected");
    assert!(!table_exists(&conn, "test_marker").unwrap());
}
