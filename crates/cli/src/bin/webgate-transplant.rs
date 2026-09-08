//! 既存 web セッションを extgate-{binding_id} へ 14 store 一回移送する。

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    let db_path = std::env::args()
        .nth(1)
        .context("usage: webgate-transplant <db-path>")?;
    let db = opencrab_db::Db::open(&db_path)?;
    let conn = db.lock().map_err(|_| anyhow::anyhow!("db lock"))?;
    let results = opencrab_db::webgate_transplant::transplant_all(&conn)?;
    for (logical, outcome) in &results {
        println!("{logical}\t{outcome:?}");
    }
    println!("mappings\t{}", results.len());
    Ok(())
}
