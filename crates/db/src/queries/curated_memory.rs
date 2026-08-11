use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// MEMORY: Curated
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratedMemoryRow {
    pub id: String,
    pub agent_id: String,
    pub category: String,
    pub content: String,
    pub created_at: String,
}

pub fn upsert_curated_memory(conn: &Connection, memory: &CuratedMemoryRow) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memory_curated (id, agent_id, category, content, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent_id, category) DO UPDATE SET
            content = excluded.content,
            updated_at = excluded.updated_at",
        params![
            memory.id,
            memory.agent_id,
            memory.category,
            memory.content,
            now,
            now,
        ],
    )?;
    Ok(())
}

/// Deletes a single curated memory entry by ID and agent_id.
/// Returns true if deleted, false if not found.
pub fn delete_curated_memory_entry(
    conn: &Connection,
    agent_id: &str,
    entry_id: &str,
) -> Result<bool> {
    let deleted = conn.execute(
        "DELETE FROM memory_curated WHERE id = ?1 AND agent_id = ?2",
        params![entry_id, agent_id],
    )?;
    Ok(deleted > 0)
}

/// category **完全一致**の curated 記憶を返す。
///
/// **本番の読み手は 2 つとも前方一致（`get_curated_memories_by_prefix`）に揃った**
/// （system プロンプト注入 = #428 / `MemoryStore::get_curated` = #544）ので、この完全一致は
/// 本番から呼ばれない。将来の実装者が誤ってこちらを選ぶ（＝ #428 / #544 の再来）のを防ぐため
/// `#[cfg(test)]` にして本番・他クレートから不可視にしてある。残す唯一の目的は
/// [`tests::curated_by_prefix_picks_up_suffixed_headings_but_not_other_prefixes`] の **foil**
/// ——「素朴な完全一致では `long_term/<見出し>` を 0 件しか引けない」を実演し、なぜ前方一致が
/// 要るのかを固定する——として使うこと。
#[cfg(test)]
pub fn get_curated_memories(
    conn: &Connection,
    agent_id: &str,
    category: &str,
) -> Result<Vec<CuratedMemoryRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, category, content, created_at FROM memory_curated
         WHERE agent_id = ?1 AND category = ?2 ORDER BY updated_at DESC",
    )?;

    let rows = stmt.query_map(params![agent_id, category], |row| {
        Ok(CuratedMemoryRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            category: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// `category` が `prefix` 完全一致、または `prefix/<見出し>` 形式の curated 記憶を全て返す。
///
/// 取り込み（`opencrab_core::import::openclaw_parser`）は long_term を **1 見出し 1 行**
/// （`long_term/<見出し>`）で入れるのに、system プロンプト注入は完全一致で引いていたため
/// `long_term/*` が 1 件も載っていなかった（#428）。両形式を拾ってまとめて注入できるようにする。
///
/// `prefix/_%`（`_` で最低 1 文字）なので、見出しが空の `prefix/` は拾わない
/// （[`list_long_term_category_seeds`](super::list_long_term_category_seeds) と同じ述語）。
/// 並びは `category` 昇順で決定的（完全一致の素の `prefix` は `prefix/…` より前に来る）。
pub fn get_curated_memories_by_prefix(
    conn: &Connection,
    agent_id: &str,
    prefix: &str,
) -> Result<Vec<CuratedMemoryRow>> {
    let like = format!("{prefix}/_%");
    let mut stmt = conn.prepare(
        "SELECT id, agent_id, category, content, created_at FROM memory_curated
         WHERE agent_id = ?1 AND (category = ?2 OR category LIKE ?3)
         ORDER BY category ASC",
    )?;

    let rows = stmt.query_map(params![agent_id, prefix, like], |row| {
        Ok(CuratedMemoryRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            category: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;

    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn list_curated_memories(
    conn: &Connection,
    agent_id: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<CuratedMemoryRow>, i64)> {
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_curated WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, agent_id, category, content, created_at FROM memory_curated
         WHERE agent_id = ?1 ORDER BY created_at ASC LIMIT ?2 OFFSET ?3",
    )?;

    let rows = stmt.query_map(params![agent_id, limit, offset], |row| {
        Ok(CuratedMemoryRow {
            id: row.get(0)?,
            agent_id: row.get(1)?,
            category: row.get(2)?,
            content: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;

    Ok((rows.collect::<std::result::Result<_, _>>()?, total))
}
