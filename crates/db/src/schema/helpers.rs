use rusqlite::Connection;

pub(super) fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// テーブルに指定カラムが存在するか判定する（将来の番号付きマイグレーション用ヘルパー）。
///
/// version 1 baseline (`migrate`) 内の約30箇所のインライン `pragma_table_info` プローブは
/// 凍結のためリファクタしないが、version 2 以降の `Migration::up` ではこのヘルパーを使う。
#[allow(dead_code)]
pub(super) fn column_exists(
    conn: &Connection,
    table: &str,
    column: &str,
) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}
