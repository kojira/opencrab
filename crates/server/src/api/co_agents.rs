use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Debug, Serialize)]
pub struct CoAgentDto {
    pub id: String,
    pub agent_id: String,
    pub co_agent_id: String,
    pub created_by: String,
    pub created_at: String,
}

fn row_to_dto(r: opencrab_db::queries::TrustedCoAgentRow) -> CoAgentDto {
    // `allowed_actions` 列はレスポンスに載せない（下記 `reject_allowed_actions` 参照）。
    CoAgentDto {
        id: r.id,
        agent_id: r.agent_id,
        co_agent_id: r.co_agent_id,
        created_by: r.created_by,
        created_at: r.created_at,
    }
}

/// #490: `allowed_actions` は権限判定に一切使われない。#485 の方針では co_agent は
/// owner 等価で、この表で解決した相手は列の中身によらず全アクションを実行できる。
/// 「絞ったつもりで登録したのに全部通る」という誤解を断つため、非空で渡されたら
/// **黙って無視せず**明示的に弾く。省略 / null / 空配列は従来どおり通す（何も絞らない）。
fn reject_allowed_actions(actions: &Option<Vec<String>>) -> Result<(), (StatusCode, String)> {
    match actions {
        Some(v) if !v.is_empty() => Err((
            StatusCode::BAD_REQUEST,
            "allowed_actions は権限判定に使われないため受け付けません。co_agent は owner \
             等価で、登録された相手は列の中身によらず全アクションを実行できます（#490）。"
                .to_string(),
        )),
        _ => Ok(()),
    }
}

pub async fn list_co_agents(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Json<Vec<CoAgentDto>> {
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_trusted_co_agents(&conn, &agent_id).unwrap_or_default();
    Json(rows.into_iter().map(row_to_dto).collect())
}

#[derive(Debug, Deserialize)]
pub struct AddCoAgentRequest {
    pub co_agent_id: String,
    /// 受け取るが権限判定には使わない。非空なら 400 で弾く（`reject_allowed_actions`）。
    pub allowed_actions: Option<Vec<String>>,
}

pub async fn add_co_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<AddCoAgentRequest>,
) -> Result<Json<CoAgentDto>, (StatusCode, String)> {
    reject_allowed_actions(&req.allowed_actions)?;

    let conn = state.db.lock().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    let row = opencrab_db::queries::TrustedCoAgentRow {
        id: id.clone(),
        agent_id: agent_id.clone(),
        co_agent_id: req.co_agent_id.clone(),
        // 権限判定に使われない列なので常に NULL で保存する。
        allowed_actions: None,
        created_by: "owner".to_string(),
        created_at: now.clone(),
    };

    opencrab_db::queries::insert_trusted_co_agent(&conn, &row)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(CoAgentDto {
        id,
        agent_id,
        co_agent_id: req.co_agent_id,
        created_by: "owner".to_string(),
        created_at: now,
    }))
}

// PATCH は撤去した（#490）。唯一の役割が `allowed_actions` の変更だったが、その列は
// 権限判定に使われず API から外したため、可変フィールドが 1 つも無くなった。何もしない
// エンドポイントを黙って残すより消すほうが誠実で、構造も単純になる（co_agent の追加/削除は
// POST/DELETE で足りる）。撤去後、この経路への PATCH は 405 Method Not Allowed を返す。

pub async fn delete_co_agent(
    State(state): State<AppState>,
    Path((agent_id, co_agent_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let conn = state.db.lock().unwrap();
    let deleted = opencrab_db::queries::delete_trusted_co_agent(&conn, &agent_id, &co_agent_id)
        .unwrap_or(false);
    Json(serde_json::json!({ "deleted": deleted }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_req(co_agent_id: &str, allowed_actions: Option<Vec<String>>) -> AddCoAgentRequest {
        AddCoAgentRequest {
            co_agent_id: co_agent_id.to_string(),
            allowed_actions,
        }
    }

    /// 非空の `allowed_actions` を渡すと 400 で弾かれ、行は 1 件も挿入されない。
    #[tokio::test]
    async fn add_rejects_non_empty_allowed_actions() {
        let state = crate::test_app_state();
        let err = add_co_agent(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(add_req("co-1", Some(vec!["execute_shell".to_string()]))),
        )
        .await
        .expect_err("非空の allowed_actions は拒否されるはず");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        // 弾いた時点で DB には触れず、行は増えない。
        let conn = state.db.lock().unwrap();
        let rows = opencrab_db::queries::list_trusted_co_agents(&conn, "agent-1").unwrap();
        assert!(rows.is_empty(), "拒否時に行が挿入されてはいけない");
    }

    /// 省略（null）なら従来どおり登録でき、保存される列は NULL。
    #[tokio::test]
    async fn add_accepts_omitted_allowed_actions() {
        let state = crate::test_app_state();
        let dto = add_co_agent(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(add_req("co-1", None)),
        )
        .await
        .expect("省略なら登録できるはず")
        .0;
        assert_eq!(dto.co_agent_id, "co-1");

        let conn = state.db.lock().unwrap();
        let rows = opencrab_db::queries::list_trusted_co_agents(&conn, "agent-1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].allowed_actions, None, "列は NULL で保存される");
    }

    /// 空配列も「何も絞らない」として通す（非空だけを弾く）。
    #[tokio::test]
    async fn add_accepts_empty_allowed_actions() {
        let state = crate::test_app_state();
        let dto = add_co_agent(
            State(state.clone()),
            Path("agent-1".to_string()),
            Json(add_req("co-1", Some(vec![]))),
        )
        .await
        .expect("空配列なら登録できるはず")
        .0;
        assert_eq!(dto.co_agent_id, "co-1");
    }
}
