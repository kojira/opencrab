use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[allow(unused_imports)]
use super::*;

// ============================================
// ノード検索（キーワード逆引き）
// ============================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexNodeSearchResult {
    pub node_id: String,
    pub short_id: Option<String>,
    pub node_type: String,
    pub source_type: String,
    pub title: String,
    pub summary: String,
    pub keywords_json: String,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub child_count: i32,
    pub score: f64,
}

/// キーワード/タイトル/要約でインデックスノードを BM25 検索する（逆引き）。
///
/// トークンは引用符でエスケープして AND 結合。0 件なら OR 結合で再検索して
/// リコールを稼ぐ（LLM が打つ複合クエリは全語一致しないことが多い）。
/// FTS は trigram トークナイザ（3 文字以上の部分一致）なので、それでも 0 件
/// かつ短い語を含むクエリは LIKE スキャンにフォールバックする（ノード表は
/// 高々数千行なので全走査でも安価）。
pub fn search_index_nodes(
    conn: &Connection,
    agent_id: &str,
    query: &str,
    limit: usize,
    node_type: Option<&str>,
) -> Result<Vec<IndexNodeSearchResult>> {
    let raw_tokens: Vec<&str> = query.split_whitespace().collect();
    if raw_tokens.is_empty() {
        return Ok(Vec::new());
    }
    let tokens: Vec<String> = raw_tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    let and_query = tokens.join(" AND ");
    let results = search_index_nodes_fts(conn, agent_id, &and_query, limit, node_type)?;
    if !results.is_empty() {
        return Ok(results);
    }
    if tokens.len() > 1 {
        let or_query = tokens.join(" OR ");
        let results = search_index_nodes_fts(conn, agent_id, &or_query, limit, node_type)?;
        if !results.is_empty() {
            return Ok(results);
        }
    }
    // trigram は 3 文字未満の語に当たらない。短い語を含む場合のみ LIKE で救済。
    if raw_tokens.iter().any(|t| t.chars().count() < 3) {
        return search_index_nodes_like(conn, agent_id, &raw_tokens, limit, node_type);
    }
    Ok(Vec::new())
}

fn search_index_nodes_like(
    conn: &Connection,
    agent_id: &str,
    tokens: &[&str],
    limit: usize,
    node_type: Option<&str>,
) -> Result<Vec<IndexNodeSearchResult>> {
    // いずれかの語が title/summary/keywords に部分一致すれば拾う（OR 相当）。
    // LIKE のメタ文字はエスケープする。
    let mut conditions = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    params_vec.push(Box::new(agent_id.to_string()));
    for token in tokens {
        let escaped = token
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let idx = params_vec.len() + 1;
        conditions.push(format!(
            "(title LIKE ?{idx} ESCAPE '\\' OR summary LIKE ?{idx} ESCAPE '\\' OR keywords_json LIKE ?{idx} ESCAPE '\\')"
        ));
        params_vec.push(Box::new(pattern));
    }
    let type_idx = params_vec.len() + 1;
    params_vec.push(Box::new(node_type.map(|s| s.to_string())));
    let limit_idx = params_vec.len() + 1;
    params_vec.push(Box::new(limit as i64));

    let sql = format!(
        "SELECT id, short_id, node_type, source_type, title, summary,
                keywords_json, date_from, date_to, child_count, 0.0 as score
         FROM memory_index_nodes
         WHERE agent_id = ?1 AND ({})
           AND (?{type_idx} IS NULL OR node_type = ?{type_idx})
         ORDER BY created_at DESC LIMIT ?{limit_idx}",
        conditions.join(" OR ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok(IndexNodeSearchResult {
            node_id: row.get(0)?,
            short_id: row.get(1)?,
            node_type: row.get(2)?,
            source_type: row.get(3)?,
            title: row.get(4)?,
            summary: row.get(5)?,
            keywords_json: row.get(6)?,
            date_from: row.get(7)?,
            date_to: row.get(8)?,
            child_count: row.get(9)?,
            score: row.get(10)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn search_index_nodes_fts(
    conn: &Connection,
    agent_id: &str,
    fts_query: &str,
    limit: usize,
    node_type: Option<&str>,
) -> Result<Vec<IndexNodeSearchResult>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.short_id, n.node_type, n.source_type, n.title, n.summary,
                n.keywords_json, n.date_from, n.date_to, n.child_count,
                bm25(memory_index_fts) as score
         FROM memory_index_fts fts
         JOIN memory_index_nodes n ON fts.node_id = n.id
         WHERE fts.agent_id = ?1 AND memory_index_fts MATCH ?2
           AND (?4 IS NULL OR n.node_type = ?4)
         ORDER BY score
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(
        params![agent_id, fts_query, limit as i64, node_type],
        |row| {
            Ok(IndexNodeSearchResult {
                node_id: row.get(0)?,
                short_id: row.get(1)?,
                node_type: row.get(2)?,
                source_type: row.get(3)?,
                title: row.get(4)?,
                summary: row.get(5)?,
                keywords_json: row.get(6)?,
                date_from: row.get(7)?,
                date_to: row.get(8)?,
                child_count: row.get(9)?,
                score: row.get(10)?,
            })
        },
    )?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// keywords 未付与の session_log topic ノードを古い順に返す（バックフィル対象）。
pub fn list_topics_missing_keywords(
    conn: &Connection,
    agent_id: &str,
    limit: usize,
) -> Result<Vec<IndexNodeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {INDEX_NODE_COLUMNS}
         FROM memory_index_nodes
         WHERE agent_id = ?1 AND node_type = 'topic' AND source_type = 'session_log'
           AND keywords_json = '[]'
         ORDER BY created_at ASC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![agent_id, limit as i64], index_node_from_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
