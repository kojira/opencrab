use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct TrustedUserDto {
    pub id: String,
    /// その経路でのユーザー識別子（#159 で `discord_user_id` から改名）。
    /// どの経路の識別子かは行の `platform`（現状 REST から登録できるのは `discord` のみ）。
    pub user_id: String,
    pub agent_id: String,
    pub permission: String,
    pub created_by: String,
    pub created_at: String,
    pub display_name: String,
}

fn row_to_dto(r: opencrab_db::queries::TrustedUserRow) -> TrustedUserDto {
    TrustedUserDto {
        id: r.id,
        user_id: r.user_id,
        agent_id: r.agent_id,
        permission: r.permission,
        created_by: r.created_by,
        created_at: r.created_at,
        display_name: r.display_name,
    }
}

pub async fn list_trusted_users(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Json<Vec<TrustedUserDto>> {
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_trusted_users(&conn, &agent_id).unwrap_or_default();
    Json(rows.into_iter().map(row_to_dto).collect())
}

#[derive(Debug, Deserialize)]
pub struct AddTrustedUserRequest {
    /// その経路でのユーザー識別子。旧キー `discord_user_id` も受け付ける（後方互換）。
    #[serde(alias = "discord_user_id")]
    pub user_id: String,
    pub permission: Option<String>,
    /// ロスター表示用の名前（ピアレビュアー一覧等）。省略時は空。
    pub display_name: Option<String>,
}

/// 信頼済みユーザーを登録する（Discord の識別子空間）。
///
/// 登録される経路は `discord` 固定（#214）。web / REST のユーザーを別経路として
/// 登録できるようにする（リクエストで `platform` を受け取る）のは #159 の後段
/// 「互換読みの撤去」とセットでやる。ここは命名の改名だけで、挙動は変えていない。
pub async fn add_trusted_user(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddTrustedUserRequest>,
) -> Result<Json<TrustedUserDto>, StatusCode> {
    let conn = state.db.lock().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let permission = req.permission.unwrap_or_else(|| "user".to_string());
    let display_name = req.display_name.unwrap_or_default();

    opencrab_db::queries::add_trusted_user(
        &conn,
        opencrab_db::queries::TRUSTED_PLATFORM_DISCORD,
        &id,
        &agent_id,
        &req.user_id,
        &permission,
        "owner",
        &now,
        &display_name,
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(TrustedUserDto {
        id,
        user_id: req.user_id,
        agent_id,
        permission,
        created_by: "owner".to_string(),
        created_at: now,
        display_name,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateTrustedUserRequest {
    pub permission: Option<String>,
    pub display_name: Option<String>,
}

pub async fn update_trusted_user(
    State(state): State<AppState>,
    Path((_agent_id, user_id)): Path<(String, String)>,
    Json(req): Json<UpdateTrustedUserRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let conn = state.db.lock().unwrap();
    // 2フィールドの更新は不可分にする（片方だけ永続化されて 500 を返さない）
    let update = || -> anyhow::Result<bool> {
        let tx = conn.unchecked_transaction()?;
        let mut updated = false;
        if let Some(ref permission) = req.permission {
            updated |=
                opencrab_db::queries::update_trusted_user_permission(&tx, &user_id, permission)?;
        }
        if let Some(ref display_name) = req.display_name {
            updated |= opencrab_db::queries::update_trusted_user_display_name(
                &tx,
                &user_id,
                display_name,
            )?;
        }
        tx.commit()?;
        Ok(updated)
    };
    let updated = update().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "updated": updated })))
}

pub async fn delete_trusted_user(
    State(state): State<AppState>,
    Path((_agent_id, user_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let deleted = opencrab_db::queries::remove_trusted_user(&conn, &user_id).unwrap_or(false);
    Json(serde_json::json!({ "deleted": deleted }))
}
