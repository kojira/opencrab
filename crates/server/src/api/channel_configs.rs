use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub guild_id: String,
}

#[derive(Debug, Serialize)]
pub struct ChannelConfigDto {
    pub channel_id: String,
    pub guild_id: String,
    pub channel_name: String,
    pub readable: bool,
    pub writable: bool,
    pub whitelisted: bool,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub guild_id: String,
    pub configs: Vec<ChannelConfigDto>,
    pub count: usize,
}

pub async fn list_channel_configs(
    State(state): State<AppState>,
    Path(_agent_id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Json<ListResponse> {
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_channel_configs_by_guild(&conn, &query.guild_id)
        .unwrap_or_default();
    let configs: Vec<ChannelConfigDto> = rows
        .into_iter()
        .map(|r| ChannelConfigDto {
            channel_id: r.channel_id,
            guild_id: r.guild_id,
            channel_name: r.channel_name,
            readable: r.readable,
            writable: r.writable,
            whitelisted: r.whitelisted,
        })
        .collect();
    let count = configs.len();
    Json(ListResponse {
        guild_id: query.guild_id,
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
}

fn default_true() -> bool {
    true
}

pub async fn upsert_channel_config(
    State(state): State<AppState>,
    Path(_agent_id): Path<String>,
    Json(req): Json<UpsertRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = state.db.lock().unwrap();
    let cfg = opencrab_db::queries::ChannelConfigRow {
        channel_id: req.channel_id.clone(),
        guild_id: req.guild_id,
        channel_name: req.channel_name,
        readable: req.readable,
        writable: req.writable,
        whitelisted: req.whitelisted,
    };
    opencrab_db::queries::upsert_channel_config(&conn, &cfg)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "channel_id": req.channel_id,
        "message": "channel config upserted"
    })))
}
