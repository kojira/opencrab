use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ListCuratedQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

pub async fn list_curated_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ListCuratedQuery>,
) -> Json<serde_json::Value> {
    let limit = query.limit.unwrap_or(100);
    let offset = query.offset.unwrap_or(0);
    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::list_curated_memories(&conn, &id, limit, offset) {
        Ok((items, total)) => Json(serde_json::json!({
            "total": total,
            "items": items,
        })),
        Err(e) => Json(serde_json::json!({
            "total": 0,
            "items": [],
            "error": e.to_string(),
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct SearchMemoryRequest {
    pub query: String,
    pub limit: Option<usize>,
}

pub async fn search_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SearchMemoryRequest>,
) -> Json<serde_json::Value> {
    let limit = req.limit.unwrap_or(10);
    let conn = state.db.lock().unwrap();

    match opencrab_db::queries::search_session_logs(&conn, &id, &req.query, limit) {
        Ok(results) => Json(serde_json::json!({
            "query": req.query,
            "count": results.len(),
            "results": results,
        })),
        Err(e) => Json(serde_json::json!({
            "error": e.to_string(),
        })),
    }
}

pub async fn get_memory_index_tree(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    match opencrab_db::queries::get_index_tree(&conn, &id) {
        Ok(nodes) => {
            // Build tree structure: group children by parent_id
            let tree: Vec<serde_json::Value> = nodes
                .iter()
                .filter(|n| n.parent_id.is_none())
                .map(|root| build_tree_node(root, &nodes))
                .collect();
            Json(serde_json::json!({
                "nodes": nodes,
                "tree": tree,
            }))
        }
        Err(e) => Json(serde_json::json!({
            "error": e.to_string(),
            "nodes": [],
            "tree": [],
        })),
    }
}

fn build_tree_node(
    node: &opencrab_db::queries::IndexNodeRow,
    all: &[opencrab_db::queries::IndexNodeRow],
) -> serde_json::Value {
    let children: Vec<serde_json::Value> = all
        .iter()
        .filter(|n| n.parent_id.as_deref() == Some(&node.id))
        .map(|child| build_tree_node(child, all))
        .collect();
    serde_json::json!({
        "id": node.id,
        "title": node.title,
        "node_type": node.node_type,
        "summary": node.summary,
        "depth": node.depth,
        "child_count": node.child_count,
        "children": children,
    })
}
