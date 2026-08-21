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
    // #421: 各ゲートフィールドは省略可（None）。省略時は既存行の値（行が無ければ
    // 現在の実効値）を保持する patch 意味論。full-replace の既定値落ちで「一部だけ
    // 変えたい」更新が他の設定を黙って壊すのを防ぐ。
    #[serde(default)]
    pub readable: Option<bool>,
    #[serde(default)]
    pub writable: Option<bool>,
    #[serde(default)]
    pub whitelisted: Option<bool>,
    #[serde(default)]
    pub heartbeat_enabled: Option<bool>,
    #[serde(default)]
    pub heartbeat_interval_secs: Option<u64>,
}

pub async fn upsert_channel_config(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<UpsertRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = state.db.lock().unwrap();
    // #421: 省略フィールドは既存行の値（行が無ければ現在の実効値）を保持する。
    let existing =
        opencrab_db::queries::get_channel_config_for_agent(&conn, &req.channel_id, &agent_id)
            .ok()
            .flatten();
    let readable = req.readable.unwrap_or_else(|| {
        opencrab_db::queries::is_channel_readable_for_agent(&conn, &req.channel_id, &agent_id)
    });
    let writable = req.writable.unwrap_or_else(|| {
        opencrab_db::queries::is_channel_writable_for_agent(&conn, &req.channel_id, &agent_id)
    });
    let whitelisted = req.whitelisted.unwrap_or_else(|| {
        opencrab_db::queries::is_channel_whitelisted_for_agent(&conn, &req.channel_id, &agent_id)
    });
    let heartbeat_enabled = req
        .heartbeat_enabled
        .or_else(|| existing.as_ref().map(|c| c.heartbeat_enabled))
        .unwrap_or(true);
    // heartbeat_interval_secs / heartbeat_instructions は既存を保持（Phase 4でUI管理するまで消さない）。
    let heartbeat_interval_secs = req
        .heartbeat_interval_secs
        .or_else(|| existing.as_ref().and_then(|c| c.heartbeat_interval_secs));
    let heartbeat_instructions = existing
        .map(|c| c.heartbeat_instructions)
        .unwrap_or_default();
    let cfg = opencrab_db::queries::ChannelConfigRow {
        channel_id: req.channel_id.clone(),
        agent_id: agent_id.clone(),
        guild_id: req.guild_id,
        channel_name: req.channel_name,
        readable,
        writable,
        whitelisted,
        heartbeat_enabled,
        heartbeat_interval_secs,
        heartbeat_instructions,
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

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_db::queries::ChannelConfigRow;

    fn seed(state: &AppState, whitelisted: bool, heartbeat_enabled: bool) {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::upsert_channel_config(
            &conn,
            &ChannelConfigRow {
                channel_id: "ch-1".into(),
                agent_id: "a1".into(),
                guild_id: "g1".into(),
                channel_name: "general".into(),
                readable: true,
                writable: true,
                whitelisted,
                heartbeat_enabled,
                heartbeat_interval_secs: Some(600),
                heartbeat_instructions: "keep".into(),
            },
        )
        .unwrap();
    }

    /// #421: 省略フィールド（None）は既存行の値を保持する（handler の patch 意味論）。
    /// full-replace のまま既定値へ落とすと、読み書きだけ変える更新で whitelist / heartbeat
    /// が黙って壊れる。
    #[tokio::test]
    async fn upsert_omitted_fields_preserve_existing_row() {
        let state = crate::test_app_state();
        seed(&state, true, false);

        // readable だけ変更し、他フィールドは省略（None）で更新する。
        let req = UpsertRequest {
            channel_id: "ch-1".into(),
            guild_id: "g1".into(),
            channel_name: "general".into(),
            readable: Some(false),
            writable: None,
            whitelisted: None,
            heartbeat_enabled: None,
            heartbeat_interval_secs: None,
        };
        let resp =
            upsert_channel_config(State(state.clone()), Path("a1".to_string()), Json(req)).await;
        assert!(resp.is_ok(), "upsert handler should succeed");

        let conn = state.db.lock().unwrap();
        let cfg = opencrab_db::queries::get_channel_config_for_agent(&conn, "ch-1", "a1")
            .unwrap()
            .unwrap();
        assert!(!cfg.readable, "明示した readable=false は反映される");
        assert!(cfg.writable, "省略した writable は既存 true を保持");
        assert!(cfg.whitelisted, "省略した whitelisted は既存 true を保持");
        assert!(
            !cfg.heartbeat_enabled,
            "省略した heartbeat_enabled は既存 false を保持"
        );
        assert_eq!(
            cfg.heartbeat_interval_secs,
            Some(600),
            "省略した interval は既存を保持"
        );
        assert_eq!(cfg.heartbeat_instructions, "keep", "指示文は既存を保持");
    }

    /// #421: JSON でゲートフィールドを省略すると None にデシリアライズされる
    /// （既定 false/true へ落ちない）。ここが Option でなくなると省略時上書きが再発する。
    #[test]
    fn upsert_request_omitted_bools_are_none() {
        let req: UpsertRequest = serde_json::from_value(serde_json::json!({
            "channel_id": "ch-1",
            "guild_id": "g1",
        }))
        .unwrap();
        assert_eq!(req.readable, None);
        assert_eq!(req.writable, None);
        assert_eq!(req.whitelisted, None);
        assert_eq!(req.heartbeat_enabled, None);
    }
}
