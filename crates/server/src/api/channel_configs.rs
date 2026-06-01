use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub guild_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChannelConfigDto {
    pub channel_id: String,
    #[serde(default)]
    pub agent_id: String,
    pub guild_id: String,
    pub channel_name: String,
    pub readable: bool,
    pub writable: bool,
    pub whitelisted: bool,
    pub heartbeat_enabled: bool,
    pub heartbeat_interval_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub agent_id: String,
    pub guild_id: String,
    pub configs: Vec<ChannelConfigDto>,
    pub count: usize,
}

pub async fn list_channel_configs(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Json<ListResponse> {
    let conn = state.db.lock().unwrap();
    let rows =
        opencrab_db::queries::list_channel_configs_by_agent(&conn, &agent_id).unwrap_or_default();
    // guild_id クエリが指定されていれば絞り込む
    let guild_filter = query.guild_id.clone().unwrap_or_default();
    let configs: Vec<ChannelConfigDto> = rows
        .into_iter()
        .filter(|r| guild_filter.is_empty() || r.guild_id == guild_filter)
        .map(|r| ChannelConfigDto {
            channel_id: r.channel_id,
            agent_id: r.agent_id,
            guild_id: r.guild_id,
            channel_name: r.channel_name,
            readable: r.readable,
            writable: r.writable,
            whitelisted: r.whitelisted,
            heartbeat_enabled: r.heartbeat_enabled,
            heartbeat_interval_secs: r.heartbeat_interval_secs,
        })
        .collect();
    let count = configs.len();
    Json(ListResponse {
        agent_id,
        guild_id: query.guild_id.unwrap_or_default(),
        configs,
        count,
    })
}

#[derive(Debug, Deserialize)]
pub struct UpsertRequest {
    pub channel_id: String,
    pub guild_id: String,
    #[serde(default)]
    pub channel_name: String,
    #[serde(default = "default_true")]
    pub readable: bool,
    #[serde(default = "default_true")]
    pub writable: bool,
    #[serde(default)]
    pub whitelisted: bool,
    #[serde(default = "default_true")]
    pub heartbeat_enabled: bool,
    pub heartbeat_interval_secs: Option<u64>,
}

fn default_true() -> bool {
    true
}

pub async fn upsert_channel_config(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<UpsertRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = state.db.lock().unwrap();
    // 既存のハートビート上書きを保持する（Phase 4でUI管理するまで消さない）。
    let existing_hb =
        opencrab_db::queries::get_channel_config_for_agent(&conn, &req.channel_id, &agent_id)
            .ok()
            .flatten()
            .map(|c| c.heartbeat_instructions)
            .unwrap_or_default();
    let cfg = opencrab_db::queries::ChannelConfigRow {
        channel_id: req.channel_id.clone(),
        agent_id: agent_id.clone(),
        guild_id: req.guild_id,
        channel_name: req.channel_name,
        readable: req.readable,
        writable: req.writable,
        whitelisted: req.whitelisted,
        heartbeat_enabled: req.heartbeat_enabled,
        heartbeat_interval_secs: req.heartbeat_interval_secs,
        heartbeat_instructions: existing_hb,
    };
    opencrab_db::queries::upsert_channel_config(&conn, &cfg)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "channel_id": req.channel_id,
        "agent_id": agent_id,
        "message": "channel config upserted"
    })))
}

pub async fn delete_channel_config(
    State(state): State<AppState>,
    Path((agent_id, channel_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = state.db.lock().unwrap();
    let deleted =
        opencrab_db::queries::delete_channel_config_for_agent(&conn, &channel_id, &agent_id)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if deleted {
        Ok(Json(serde_json::json!({
            "channel_id": channel_id,
            "message": "channel config deleted"
        })))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}
