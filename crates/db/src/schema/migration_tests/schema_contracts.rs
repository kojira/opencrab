use super::super::*;
use rusqlite::Connection;
/// H. SCHEMA_SQL 側と TASK_LEDGER_SQL 側で生成されるテーブル定義が一致する
/// （両所への二重記載がドリフトしていないことの検証）。
#[test]
fn task_ledger_schema_parity() {
    let dump = |conn: &Connection| -> Vec<String> {
        conn.prepare(
            "SELECT sql FROM sqlite_master
                 WHERE name IN ('task_ledger','task_progress',
                                'idx_task_ledger_session','idx_task_ledger_one_active',
                                'idx_task_progress_task')
                 ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    };

    // 新規DB: SCHEMA_SQL 由来（baseline 時点でテーブルが出来ており、v2 は no-op）。
    let fresh = crate::init_memory().expect("fresh");
    // 既存DB: baseline 後に v2 マイグレーション由来で作成。
    let migrated = crate::init_memory().expect("migrated");
    migrated
        .execute_batch("DROP TABLE task_progress; DROP TABLE task_ledger; PRAGMA user_version = 1")
        .unwrap();
    initialize(&migrated).expect("re-migrate");

    assert_eq!(dump(&fresh), dump(&migrated));
    assert_eq!(dump(&fresh).len(), 5, "expected 2 tables + 3 indexes");
}
