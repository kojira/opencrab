/// GET /api/agents/{id}/memory/index — インデックス状態取得
pub async fn get_memory_index_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let watermark = opencrab_db::queries::get_index_watermark(&conn, &id)
        .ok()
        .flatten();
    let unindexed = opencrab_db::queries::get_unindexed_log_count(&conn, &id).unwrap_or(0);
    let tree = opencrab_db::queries::get_index_tree(&conn, &id).unwrap_or_default();
    let config = opencrab_db::queries::get_memory_index_config(&conn, &id).unwrap_or_else(|_| {
        opencrab_db::queries::AgentMemoryIndexConfig {
            agent_id: id.clone(),
            batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
            threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
            updated_at: String::new(),
        }
    });

    Json(serde_json::json!({
        "agent_id": id,
        "total_nodes": tree.len(),
        "unindexed_logs": unindexed,
        "watermark": watermark,
        "node_type_counts": {
            "root": tree.iter().filter(|n| n.node_type == "root").count(),
            "period": tree.iter().filter(|n| n.node_type == "period").count(),
            "session": tree.iter().filter(|n| n.node_type == "session").count(),
            "topic": tree.iter().filter(|n| n.node_type == "topic").count(),
        },
        "config": {
            "batch_size": config.batch_size,
            "threshold": config.threshold,
            "batch_size_min": opencrab_db::queries::BATCH_SIZE_MIN,
            "threshold_min": opencrab_db::queries::THRESHOLD_MIN,
        },
    }))
}

/// POST /api/agents/{id}/memory/index — 手動インデックス構築トリガー
pub async fn trigger_memory_index_build(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let db = state.db.clone();
    let agent_id = id.clone();
    let llm_router = state.llm_router.clone();
    let model = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::effective_model_for_agent(&conn, &agent_id, &state.default_model)
            .unwrap_or_else(|_| state.default_model.clone())
    };

    let config = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_memory_index_config(&conn, &agent_id).unwrap_or_else(|_| {
            opencrab_db::queries::AgentMemoryIndexConfig {
                agent_id: agent_id.clone(),
                batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                updated_at: String::new(),
            }
        })
    };

    let (persona_name, personality) = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent(&conn, &agent_id)
            .ok()
            .flatten()
            .map(|a| (a.persona_name, a.personality))
            .unwrap_or_default()
    };

    let llm_adapter = crate::llm_adapter::LlmRouterAdapter::new(llm_router);

    match opencrab_core::memory_index::IndexBuilder::build_incremental(
        &db,
        &agent_id,
        &llm_adapter,
        &model,
        config.batch_size as usize,
        &persona_name,
        personality.as_deref(),
    )
    .await
    {
        Ok(result) => Json(serde_json::json!({
            "ok": true,
            "nodes_created": result.nodes_created,
            "logs_indexed": result.logs_indexed,
        })),
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        })),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateMemoryIndexConfigRequest {
    pub batch_size: Option<i64>,
    pub threshold: Option<i64>,
}

pub async fn update_memory_index_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateMemoryIndexConfigRequest>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let current = opencrab_db::queries::get_memory_index_config(&conn, &id).unwrap_or_else(|_| {
        opencrab_db::queries::AgentMemoryIndexConfig {
            agent_id: id.clone(),
            batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
            threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
            updated_at: String::new(),
        }
    });

    let new_batch_size = req.batch_size.unwrap_or(current.batch_size);
    let new_threshold = req.threshold.unwrap_or(current.threshold);

    match opencrab_db::queries::upsert_memory_index_config(
        &conn,
        &id,
        new_batch_size,
        new_threshold,
    ) {
        Ok(config) => Json(serde_json::json!({
            "ok": true,
            "config": {
                "agent_id": config.agent_id,
                "batch_size": config.batch_size,
                "threshold": config.threshold,
                "updated_at": config.updated_at,
            }
        })),
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        })),
    }
}

/// DELETE /api/agents/{id}/memory/index — インデックス全削除
pub async fn delete_memory_index(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    match opencrab_core::memory_index::IndexBuilder::delete_index(&state.db, &id) {
        Ok(()) => Json(serde_json::json!({
            "ok": true,
            "message": "Index deleted",
        })),
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        })),
    }
}

/// POST /api/agents/{id}/memory/index/rebuild — インデックス再構築
pub async fn rebuild_memory_index(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let db = state.db.clone();
    let agent_id = id.clone();
    let llm_router = state.llm_router.clone();
    let model = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::effective_model_for_agent(&conn, &agent_id, &state.default_model)
            .unwrap_or_else(|_| state.default_model.clone())
    };

    let config = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_memory_index_config(&conn, &agent_id).unwrap_or_else(|_| {
            opencrab_db::queries::AgentMemoryIndexConfig {
                agent_id: agent_id.clone(),
                batch_size: opencrab_db::queries::BATCH_SIZE_DEFAULT,
                threshold: opencrab_db::queries::THRESHOLD_DEFAULT,
                updated_at: String::new(),
            }
        })
    };

    let (persona_name, personality) = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent(&conn, &agent_id)
            .ok()
            .flatten()
            .map(|a| (a.persona_name, a.personality))
            .unwrap_or_default()
    };

    let llm_adapter = crate::llm_adapter::LlmRouterAdapter::new(llm_router);

    match opencrab_core::memory_index::IndexBuilder::rebuild_index(
        &db,
        &agent_id,
        &llm_adapter,
        &model,
        config.batch_size as usize,
        &persona_name,
        personality.as_deref(),
    )
    .await
    {
        Ok(result) => Json(serde_json::json!({
            "ok": true,
            "nodes_created": result.nodes_created,
            "logs_indexed": result.logs_indexed,
        })),
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        })),
    }
}

/// POST /api/agents/{id}/memory/index/merge — トピック再マージ
pub async fn merge_memory_index_topics(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let db = state.db.clone();
    let agent_id = id.clone();
    let llm_router = state.llm_router.clone();
    let model = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::effective_model_for_agent(&conn, &agent_id, &state.default_model)
            .unwrap_or_else(|_| state.default_model.clone())
    };

    let (persona_name, personality) = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent(&conn, &agent_id)
            .ok()
            .flatten()
            .map(|a| (a.persona_name, a.personality))
            .unwrap_or_default()
    };

    let llm_adapter = crate::llm_adapter::LlmRouterAdapter::new(llm_router);
    // デフォルト: periodあたり最大10topic
    let max_topics_per_period = 10usize;

    match opencrab_core::memory_index::IndexBuilder::merge_topics(
        &db,
        &agent_id,
        &llm_adapter,
        &model,
        max_topics_per_period,
        &persona_name,
        personality.as_deref(),
    )
    .await
    {
        Ok(result) => Json(serde_json::json!({
            "ok": true,
            "periods_processed": result.periods_processed,
            "topics_merged": result.topics_merged,
            "topics_deleted": result.topics_deleted,
        })),
        Err(e) => Json(serde_json::json!({
            "ok": false,
            "error": e.to_string(),
        })),
    }
}

