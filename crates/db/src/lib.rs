pub mod schema;
pub mod queries;

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// データベース接続を初期化
pub fn init_connection(path: &str) -> Result<Connection> {
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;

    // WALモード有効化
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;

    // スキーマ初期化
    schema::initialize(&conn)?;

    Ok(conn)
}

/// インメモリDB（テスト用）
pub fn init_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    schema::initialize(&conn)?;
    Ok(conn)
}
