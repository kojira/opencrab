//! 定時実行（#455）の CRUD API（設計 §7.4）。
//!
//! `GET/POST /api/agents/{id}/schedules`, `PATCH/DELETE /api/schedules/{sid}`。
//! **既存のダッシュボード系エージェント設定 API（`channel_configs` 等）と同じ認証層の内側**
//! に置く（新しい認可ゲートは足さない・設計 §10.2）。この CRUD（owner/dashboard 用）と、
//! **エージェント自身が触るツール**（`crate::agent_schedule` の `set_my_schedule` /
//! `get_my_schedules`・オーナー裁定 2026-08-09 で §7.4 の「ツールを追加しない」を撤回）は
//! **検証・登録ロジックを共有する**（[`create_schedule_core`] / [`list_session_schedules_core`]）。
//!
//! # 語彙・持ち方（統括裁定・v38）
//! 応答の**次回発火は `next_fire_at`**（heartbeat と同名）で、**列に持たず照会時に算出**する
//! （[`crate::schedule_cron::schedule_next_fire_at`]）。cron 式・tz・enabled を変えても古い
//! キャッシュが残らない（stale フリー）。
//!
//! # cron 検証
//! cron / `@every` / timezone は保存前に [`crate::schedule_cron::validate_schedule`] で検証し、
//! 不正なら **400**（実行時も fail-closed で発火対象外）。
//!
//! # 発火先の制約
//! `session_id` は**そのエージェントの発火経路を持つセッション**（`nostr-{agent}` /
//! `discord-{agent}-{guild}-{channel}`）に限る（[`resolve_session_fire_target`] が `Some`）。
//! 「登録できたのに永遠に発火しない行」や他エージェントのセッションを作らせない。

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::schedule_cron::{schedule_next_fire_at, validate_schedule};
use crate::AppState;
use chrono::{DateTime, Utc};
use opencrab_db::queries::{resolve_session_fire_target, AgentScheduleRow};

fn default_timezone() -> String {
    "Asia/Tokyo".to_string()
}

/// スケジュール 1 件の応答表現。`next_fire_at` は照会時算出（キャッシュ列なし）。
#[derive(Debug, Serialize)]
pub struct ScheduleDto {
    pub id: i64,
    pub agent_id: String,
    pub session_id: String,
    pub cron_expr: String,
    pub timezone: String,
    pub message: String,
    pub enabled: bool,
    pub anchor_at: Option<String>,
    pub last_fired_at: Option<String>,
    /// 次回発火時刻（rfc3339 UTC）。cron/`@every` から算出。解釈不能なら `null`。
    pub next_fire_at: Option<String>,
    /// enabled なのに発火しない状態か（heartbeat の `gated` と同じ扱い）。
    pub gated: bool,
    /// `gated=true` のときの理由。**schedule は G ゲートの対象外**なので理由は「式を解釈
    /// できない」等に限られる（G による沈黙は起きない）。
    pub gated_reason: Option<String>,
}

/// CRUD ハンドラとエージェント向けツールが**共有する**操作エラー（ロジックを二重化しない）。
pub(crate) enum ScheduleOpError {
    /// 入力不正（cron/tz/session/message）。ハンドラは 400、ツールは remedy 付きエラーへ写す。
    BadRequest(String),
    /// サーバ内部エラー（DB 等）。
    Internal(String),
}

fn parse_wall_clock(s: &Option<String>) -> Option<DateTime<Utc>> {
    let s = s.as_ref()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

impl ScheduleDto {
    pub(crate) fn from_row(row: AgentScheduleRow) -> Self {
        // 照会時算出（真実は再計算・キャッシュしない）。
        let next_fire_at = schedule_next_fire_at(
            &row.cron_expr,
            &row.timezone,
            parse_wall_clock(&row.anchor_at),
            parse_wall_clock(&row.last_fired_at),
        )
        .ok()
        .flatten()
        .map(|t| t.to_rfc3339());
        // enabled なのに next を算出できない = 発火しない（式が解釈不能など）。schedule は G ゲート
        // 対象外なので、gated の理由はこれに限られる（heartbeat の G 沈黙は起きない）。
        let gated_reason = if row.enabled && next_fire_at.is_none() {
            Some(format!(
                "スケジュール式（{}）を解釈できないため発火しません。cron 5 フィールド（例: 0 7 * * *）か @every 形式（例: @every 3h）で指定し直してください。",
                row.cron_expr
            ))
        } else {
            None
        };
        ScheduleDto {
            id: row.id.unwrap_or_default(),
            agent_id: row.agent_id,
            session_id: row.session_id,
            cron_expr: row.cron_expr,
            timezone: row.timezone,
            message: row.message,
            enabled: row.enabled,
            anchor_at: row.anchor_at,
            last_fired_at: row.last_fired_at,
            next_fire_at,
            gated: gated_reason.is_some(),
            gated_reason,
        }
    }
}

/// 新規スケジュールを検証・登録して DTO を返す共有コア（CRUD ハンドラ / ツールが共用）。
///
/// cron/`@every`/timezone を検証し、`session_id` がそのエージェントの発火経路を持つことを確認し、
/// enabled なら `anchor_at=now`（設計 §4.4）で挿入する。成功後に `scheduler_wake` を鳴らす（#437）。
pub(crate) fn create_schedule_core(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    cron_expr: &str,
    timezone: &str,
    message: &str,
    enabled: bool,
) -> Result<ScheduleDto, ScheduleOpError> {
    validate_schedule(cron_expr, timezone).map_err(|e| {
        ScheduleOpError::BadRequest(format!(
            "スケジュール式または timezone が不正です（{e}）。cron は 5 フィールド（例: 0 7 * * *）、周期は @every 3h の形式、timezone は Asia/Tokyo のような IANA 名で指定してください。"
        ))
    })?;
    if resolve_session_fire_target(session_id, agent_id).is_none() {
        return Err(ScheduleOpError::BadRequest(
            "このセッションには発火経路がありません（Nostr の自発投稿、または Discord チャンネルのセッションでのみ登録できます）。".to_string(),
        ));
    }
    if message.trim().is_empty() {
        return Err(ScheduleOpError::BadRequest(
            "message は空にできません（発火時にエージェントへ渡す指示文を書いてください）。"
                .to_string(),
        ));
    }

    // enabled で作るなら anchor=now（初回発火を「now 以降の最初のスロット / now+周期」へ）。
    let anchor_at = if enabled {
        Some(Utc::now().to_rfc3339())
    } else {
        None
    };
    let row = AgentScheduleRow {
        id: None,
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        cron_expr: cron_expr.to_string(),
        timezone: timezone.to_string(),
        message: message.to_string(),
        enabled,
        anchor_at,
        last_fired_at: None,
    };
    let id = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::insert_agent_schedule(&conn, &row)
            .map_err(|e| ScheduleOpError::Internal(e.to_string()))?
    };
    // 即時反映（#437）: スケジューラを起こして rebuild させる。
    state.scheduler_wake.notify_one();
    let saved = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_schedule(&conn, id)
            .map_err(|e| ScheduleOpError::Internal(e.to_string()))?
            .ok_or_else(|| ScheduleOpError::Internal("insert 後の行が見つかりません".to_string()))?
    };
    Ok(ScheduleDto::from_row(saved))
}

/// あるセッションに属するスケジュールを DTO で列挙する共有コア（ツール `get_my_schedules` 用）。
pub(crate) fn list_session_schedules_core(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
) -> Result<Vec<ScheduleDto>, ScheduleOpError> {
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_agent_schedules(&conn, agent_id)
        .map_err(|e| ScheduleOpError::Internal(e.to_string()))?;
    Ok(rows
        .into_iter()
        .filter(|r| r.session_id == session_id)
        .map(ScheduleDto::from_row)
        .collect())
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub agent_id: String,
    pub schedules: Vec<ScheduleDto>,
    pub count: usize,
}

/// `GET /api/agents/{id}/schedules` — あるエージェントの全スケジュール。
pub async fn list_schedules(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<ListResponse>, StatusCode> {
    let conn = state.db.lock().unwrap();
    let rows = opencrab_db::queries::list_agent_schedules(&conn, &agent_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let schedules: Vec<ScheduleDto> = rows.into_iter().map(ScheduleDto::from_row).collect();
    let count = schedules.len();
    Ok(Json(ListResponse {
        agent_id,
        schedules,
        count,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub session_id: String,
    pub cron_expr: String,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub message: String,
    /// 既定は無効（fail-closed / #240）。
    #[serde(default)]
    pub enabled: bool,
}

/// `POST /api/agents/{id}/schedules` — 新規スケジュールを登録する。
///
/// cron/tz を検証し（不正 400）、`session_id` がそのエージェントの発火経路を持つことを確認する
/// （不正 400）。enabled で作るときは `anchor_at=now`（有効化起点・設計 §4.4）。
pub async fn create_schedule(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<CreateRequest>,
) -> Result<Json<ScheduleDto>, StatusCode> {
    // 検証・登録・wake は共有コアへ（エージェント向けツール set_my_schedule と同一ロジック）。
    match create_schedule_core(
        &state,
        &agent_id,
        &req.session_id,
        &req.cron_expr,
        &req.timezone,
        &req.message,
        req.enabled,
    ) {
        Ok(dto) => Ok(Json(dto)),
        Err(ScheduleOpError::BadRequest(_)) => Err(StatusCode::BAD_REQUEST),
        Err(ScheduleOpError::Internal(_)) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Debug, Deserialize)]
pub struct PatchRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cron_expr: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `PATCH /api/schedules/{sid}` — 既存スケジュールを部分更新する。
///
/// **アンカーの向き（設計 §4.4）**:
/// - cron 式 / timezone の**明示変更**、または **無効→有効化**では `anchor_at=now`・
///   `last_fired_at=NULL`（新しい式で「now 以降の最初のスロット / now+周期」から始める）。
/// - **有効→無効化**では anchor/last_fired を**触らない**（意図した疎らさを壊さない）。
/// - message だけの変更では時刻系を触らない。
pub async fn update_schedule(
    State(state): State<AppState>,
    Path(sid): Path<i64>,
    Json(req): Json<PatchRequest>,
) -> Result<Json<ScheduleDto>, StatusCode> {
    let existing = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_schedule(&conn, sid)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::NOT_FOUND)?
    };

    let new_session_id = req
        .session_id
        .unwrap_or_else(|| existing.session_id.clone());
    let new_cron = req.cron_expr.unwrap_or_else(|| existing.cron_expr.clone());
    let new_tz = req.timezone.unwrap_or_else(|| existing.timezone.clone());
    let new_message = req.message.unwrap_or_else(|| existing.message.clone());
    let new_enabled = req.enabled.unwrap_or(existing.enabled);

    // 検証（不正は 400）。
    validate_schedule(&new_cron, &new_tz).map_err(|_| StatusCode::BAD_REQUEST)?;
    if resolve_session_fire_target(&new_session_id, &existing.agent_id).is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if new_message.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // アンカーの向き（§4.4）。
    let timing_changed = new_cron != existing.cron_expr || new_tz != existing.timezone;
    let enabling = new_enabled && !existing.enabled;
    let (anchor_at, last_fired_at) = if timing_changed || enabling {
        // 明示変更 / 有効化 → now を起点にし直す（last_fired は捨てて次スロットから）。
        (Some(Utc::now().to_rfc3339()), None)
    } else {
        // それ以外（無効化・message 変更・変化なし）→ 位相を保存（触らない）。
        (existing.anchor_at.clone(), existing.last_fired_at.clone())
    };

    let row = AgentScheduleRow {
        id: Some(sid),
        agent_id: existing.agent_id.clone(),
        session_id: new_session_id,
        cron_expr: new_cron,
        timezone: new_tz,
        message: new_message,
        enabled: new_enabled,
        anchor_at,
        last_fired_at,
    };
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::update_agent_schedule(&conn, &row)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    state.scheduler_wake.notify_one();

    let saved = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_schedule(&conn, sid)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
    };
    Ok(Json(ScheduleDto::from_row(saved)))
}

/// `DELETE /api/schedules/{sid}` — スケジュールを削除する。
pub async fn delete_schedule(
    State(state): State<AppState>,
    Path(sid): Path<i64>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    {
        let conn = state.db.lock().unwrap();
        let existing = opencrab_db::queries::get_agent_schedule(&conn, sid)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        if existing.is_none() {
            return Err(StatusCode::NOT_FOUND);
        }
        opencrab_db::queries::delete_agent_schedule(&conn, sid)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    state.scheduler_wake.notify_one();
    Ok(Json(serde_json::json!({
        "id": sid,
        "message": "schedule deleted"
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "6b79ac3a-7f17-4618-a827-5bda992a3698";

    fn state_with_db() -> AppState {
        crate::test_app_state()
    }

    fn nostr_session() -> String {
        format!("nostr-{AGENT}")
    }

    #[tokio::test]
    async fn create_rejects_bad_cron() {
        let state = state_with_db();
        let req = CreateRequest {
            session_id: nostr_session(),
            cron_expr: "not a cron".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            message: "hi".to_string(),
            enabled: true,
        };
        let res = create_schedule(State(state), Path(AGENT.to_string()), Json(req)).await;
        assert_eq!(res.err(), Some(StatusCode::BAD_REQUEST));
    }

    #[tokio::test]
    async fn create_rejects_foreign_session() {
        let state = state_with_db();
        // 別エージェントの session を渡す → resolve が None → 400。
        let req = CreateRequest {
            session_id: "nostr-other-agent".to_string(),
            cron_expr: "0 7 * * *".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            message: "hi".to_string(),
            enabled: true,
        };
        let res = create_schedule(State(state), Path(AGENT.to_string()), Json(req)).await;
        assert_eq!(res.err(), Some(StatusCode::BAD_REQUEST));
    }

    #[tokio::test]
    async fn create_then_list_computes_next_fire_at() {
        let state = state_with_db();
        let req = CreateRequest {
            session_id: nostr_session(),
            cron_expr: "@every 3h".to_string(),
            timezone: "Asia/Tokyo".to_string(),
            message: "巡回してください".to_string(),
            enabled: true,
        };
        let created = create_schedule(State(state.clone()), Path(AGENT.to_string()), Json(req))
            .await
            .expect("create ok")
            .0;
        assert!(created.id > 0);
        assert!(created.enabled);
        assert!(created.anchor_at.is_some(), "enabled 作成で anchor=now");
        // @every 3h・anchor=now → next_fire_at は算出され未来（now+3h）。
        assert!(
            created.next_fire_at.is_some(),
            "next_fire_at が照会時算出される（列に持たない）"
        );

        let listed = list_schedules(State(state), Path(AGENT.to_string()))
            .await
            .expect("list ok")
            .0;
        assert_eq!(listed.count, 1);
        assert_eq!(listed.schedules[0].id, created.id);
    }

    #[tokio::test]
    async fn patch_disable_stops_and_keeps_phase_then_delete() {
        let state = state_with_db();
        let created = create_schedule(
            State(state.clone()),
            Path(AGENT.to_string()),
            Json(CreateRequest {
                session_id: nostr_session(),
                cron_expr: "@every 3h".to_string(),
                timezone: "Asia/Tokyo".to_string(),
                message: "x".to_string(),
                enabled: true,
            }),
        )
        .await
        .unwrap()
        .0;
        let anchor_before = created.anchor_at.clone();

        // enabled=false で停止。無効化では anchor を触らない（位相保存）。
        let patched = update_schedule(
            State(state.clone()),
            Path(created.id),
            Json(PatchRequest {
                session_id: None,
                cron_expr: None,
                timezone: None,
                message: None,
                enabled: Some(false),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(!patched.enabled, "enabled=false で停止");
        assert_eq!(
            patched.anchor_at, anchor_before,
            "無効化で anchor を触らない"
        );

        // 削除 → 以後 list は空。
        let _ = delete_schedule(State(state.clone()), Path(created.id))
            .await
            .expect("delete ok");
        let listed = list_schedules(State(state), Path(AGENT.to_string()))
            .await
            .unwrap()
            .0;
        assert_eq!(listed.count, 0);
    }

    #[tokio::test]
    async fn patch_cron_change_resets_anchor() {
        let state = state_with_db();
        let created = create_schedule(
            State(state.clone()),
            Path(AGENT.to_string()),
            Json(CreateRequest {
                session_id: nostr_session(),
                cron_expr: "@every 3h".to_string(),
                timezone: "Asia/Tokyo".to_string(),
                message: "x".to_string(),
                enabled: true,
            }),
        )
        .await
        .unwrap()
        .0;

        // last_fired を刻んでおく（発火済みを模す）。
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::set_agent_schedule_last_fired(
                &conn,
                created.id,
                "2026-01-01T00:00:00Z",
            )
            .unwrap();
        }

        // cron 式を変更 → anchor=now・last_fired=NULL にリセットされる（明示変更）。
        let patched = update_schedule(
            State(state),
            Path(created.id),
            Json(PatchRequest {
                session_id: None,
                cron_expr: Some("0 7 * * *".to_string()),
                timezone: None,
                message: None,
                enabled: None,
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(patched.cron_expr, "0 7 * * *");
        assert_eq!(
            patched.last_fired_at, None,
            "cron 変更で last_fired をリセット（新しい式で次スロットから）"
        );
        assert!(patched.anchor_at.is_some(), "cron 変更で anchor=now");
    }

    #[tokio::test]
    async fn patch_missing_is_404() {
        let state = state_with_db();
        let res = update_schedule(
            State(state),
            Path(99999),
            Json(PatchRequest {
                session_id: None,
                cron_expr: None,
                timezone: None,
                message: Some("x".to_string()),
                enabled: None,
            }),
        )
        .await;
        assert_eq!(res.err(), Some(StatusCode::NOT_FOUND));
    }
}
