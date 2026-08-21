//! 手動 `VACUUM` コマンド（#337）。
//!
//! アーカイブループ（`llm_log_archive`）は行を削除するだけで、SQLite のファイルは
//! 縮まない（消えた領域は空きページとして再利用されるだけ）。実ファイルサイズを縮めて
//! ディスクを解放するには `VACUUM` が要るが、3.4GB 級の DB では長時間 DB 全体をロック
//! するため**稼働中に走らせるべきでない**。そこで自動ループには入れず、この独立コマンド
//! として分けてある。
//!
//! 使い方（**サーバを停止してから**実行する）:
//!
//! ```text
//! cargo run --bin opencrab-vacuum
//! ```
//!
//! DB パスは `config/default.toml` の `[database] path` を読む。

use anyhow::{Context, Result};
use rusqlite::Connection;

fn file_size_bytes(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1}{}", UNITS[i])
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cfg = opencrab_server::config::load_config("config/default.toml")
        .context("failed to load config/default.toml")?;
    let path = cfg.database.path;

    let before = file_size_bytes(&path);
    if let Some(b) = before {
        println!("DB: {path} ({} before VACUUM)", human(b));
    } else {
        println!("DB: {path} (size unknown before VACUUM)");
    }
    println!("Running VACUUM... (this locks the DB; the server must be stopped)");

    // プールではなく単一の生接続で開く。スキーマ初期化は不要（既存 DB を開くだけ）。
    let conn = Connection::open(&path).with_context(|| format!("open DB: {path}"))?;
    conn.execute_batch("VACUUM;").context("VACUUM failed")?;

    let after = file_size_bytes(&path);
    match (before, after) {
        (Some(b), Some(a)) => {
            let reclaimed = b.saturating_sub(a);
            println!(
                "Done. {} -> {} (reclaimed {})",
                human(b),
                human(a),
                human(reclaimed)
            );
        }
        (_, Some(a)) => println!("Done. Now {}", human(a)),
        _ => println!("Done."),
    }
    Ok(())
}
