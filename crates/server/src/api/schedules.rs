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
//! `session_id` は**そのエージェントの発火経路を持つセッション**（登録済み transport のセッション。
//! 例: `nostr-{agent}` / `discord-{agent}-{guild}-{channel}` / `web-{agent}-{conversation}`）に限る
//! （transport 登録簿の `resolve_target` が `Some`・#628）。列挙を固定せず、transport を足しても
//! 腐らない中立表現にする。「登録できたのに永遠に発火しない行」や他エージェントのセッションを作らせない。

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::schedule_cron::{schedule_next_fire_at, validate_schedule};
use crate::AppState;
use chrono::{DateTime, Utc};
use opencrab_db::queries::AgentScheduleRow;

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
#[derive(Debug)]
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

/// スケジュールを検証・**冪等に**登録して DTO を返す共有コア（CRUD ハンドラ / ツールが共用）。
///
/// cron/`@every`/timezone を検証し、`session_id` がそのエージェントの発火経路を持つことを確認する。
///
/// **冪等性（同じことを 2 回言っても 1 回）**: `(session_id, cron_expr, message)` が**完全一致**する
/// 既存行があれば**新規作成せず**その行を更新して**同じ id を返す**。同じ内容の再登録（omoikane が
/// 巡回指示を再送するたびに `set_my_schedule` が呼ばれる等）でも enabled 行が増えず、同一スロットの
/// 二重発火が起きない。**cron だけでは dedup しない**——message が違えば別スケジュール（「毎朝 7 時に
/// まとめ」と「毎朝 7 時に巡回」は別物）。制約の追加ではなく当たり前の意味論（できることは減らない）。
///
/// 成功後に `scheduler_wake` を鳴らす（#437）。
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
    if state
        .timed_fire_router
        .resolve_target(session_id, agent_id)
        .is_none()
    {
        // remedy は登録済み transport から生成する（#628・手書きしない）。
        return Err(ScheduleOpError::BadRequest(format!(
            "このセッションには発火経路がありません（{} のセッションでのみ登録できます）。",
            state.timed_fire_router.fire_target_hint()
        )));
    }
    if message.trim().is_empty() {
        return Err(ScheduleOpError::BadRequest(
            "message は空にできません（発火時にエージェントへ渡す指示文を書いてください）。"
                .to_string(),
        ));
    }

    let now = Utc::now().to_rfc3339();

    // 冪等: (session_id, cron_expr, message) 完全一致の既存行を探す（cron だけでは dedup しない）。
    let existing = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::list_agent_schedules(&conn, agent_id)
            .map_err(|e| ScheduleOpError::Internal(e.to_string()))?
            .into_iter()
            .find(|r| {
                r.session_id == session_id && r.cron_expr == cron_expr && r.message == message
            })
    };

    let saved = if let Some(mut row) = existing {
        // 一致行を更新（同じ id を返す）。**位相の向き（設計 §4.4）**:
        //   - 無効→有効化（enabling）: anchor=now・last_fired=NULL（新しく回し始める）。
        //   - 既に有効で同じ内容の再登録: **位相を保存**（触らない）——さもないと set のたびに
        //     next_fire が動いて「同じことを 2 回言うと変わる」ことになり冪等でなくなる。
        //     （ただし anchor が欠けていれば打つ。）
        let enabling = enabled && !row.enabled;
        if enabling {
            row.anchor_at = Some(now.clone());
            row.last_fired_at = None;
        } else if enabled && row.anchor_at.is_none() {
            row.anchor_at = Some(now.clone());
        }
        row.enabled = enabled;
        row.timezone = timezone.to_string();
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::update_agent_schedule(&conn, &row)
                .map_err(|e| ScheduleOpError::Internal(e.to_string()))?;
        }
        state.scheduler_wake.notify_one();
        let id = row.id.unwrap_or_default();
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_schedule(&conn, id)
            .map_err(|e| ScheduleOpError::Internal(e.to_string()))?
            .ok_or_else(|| ScheduleOpError::Internal("更新後の行が見つかりません".to_string()))?
    } else {
        // 新規: enabled で作るなら anchor=now（初回発火を「now 以降の最初のスロット / now+周期」へ）。
        let anchor_at = if enabled { Some(now.clone()) } else { None };
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
        state.scheduler_wake.notify_one();
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

/// 更新時のアンカーの向き（設計 §4.4）を計算する共通ロジック。
///
/// cron 式 / timezone の**明示変更**、または **無効→有効化**では `anchor_at=now`・
/// `last_fired_at=NULL`（新しい式で「now 以降の最初のスロット / now+周期」から始める）。
/// それ以外（無効化・message だけの変更・変化なし）は**位相を保存**（触らない）。
/// dashboard の PATCH（[`update_schedule`]）とエージェント向け `update_my_schedule`
/// （[`update_schedule_core`]）が**同じ規則**を共有する（§4.4 を二重に書かない）。
fn next_anchor_and_last_fired(
    existing: &AgentScheduleRow,
    new_cron: &str,
    new_tz: &str,
    new_enabled: bool,
) -> (Option<String>, Option<String>) {
    let timing_changed = new_cron != existing.cron_expr || new_tz != existing.timezone;
    let enabling = new_enabled && !existing.enabled;
    if timing_changed || enabling {
        (Some(Utc::now().to_rfc3339()), None)
    } else {
        (existing.anchor_at.clone(), existing.last_fired_at.clone())
    }
}

/// **所属チェック付き**で id からスケジュール行を取り出す（エージェント向けツール専用）。
///
/// 行が存在し、かつ **`agent_id` と `session_id` の両方が一致**するときだけ `Ok`。一致しない
/// （他エージェント・他セッションのもの）や、そもそも存在しない id は、**存在を明かさず**一律の
/// `BadRequest` にする。`get_my_schedules` が返した id をそのまま渡す想定で、**id を推測して
/// 他人・他セッションのスケジュールを覗いたり消したりできない**ことを保証する（#477 の決定事項 1）。
fn load_owned_schedule(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    id: i64,
) -> Result<AgentScheduleRow, ScheduleOpError> {
    let row = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_schedule(&conn, id)
            .map_err(|e| ScheduleOpError::Internal(e.to_string()))?
    };
    match row {
        Some(r) if r.agent_id == agent_id && r.session_id == session_id => Ok(r),
        // 存在しない／他人のもの／他セッションのもの、を区別せず同じ文言にする（存在秘匿）。
        _ => Err(ScheduleOpError::BadRequest(
            "指定した id のスケジュールが見つかりません（このセッションのあなたのスケジュールではありません）。get_my_schedules で id を確認してください。".to_string(),
        )),
    }
}

/// `update_schedule_core` への部分更新フィールド。各 `None` は「現在の値を保つ」。
///
/// `session_id` は**含めない**（別セッションへの付け替えを構造的に不可能にする）。
#[derive(Debug, Default)]
pub(crate) struct SchedulePatch<'a> {
    pub cron_expr: Option<&'a str>,
    pub timezone: Option<&'a str>,
    pub message: Option<&'a str>,
    pub enabled: Option<bool>,
}

/// 自分のスケジュールを **id 指定で**部分更新する共有コア（ツール `update_my_schedule` 用）。
///
/// `agent_id`＋`session_id` の所属チェック（[`load_owned_schedule`]）を通った行だけを更新する。
/// **`session_id` は変更しない**（別セッションへ付け替えさせない）。cron/tz を検証し、アンカーの
/// 向きは dashboard PATCH と同じ（[`next_anchor_and_last_fired`]）。成功後 `scheduler_wake`（#437）。
pub(crate) fn update_schedule_core(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    id: i64,
    patch: SchedulePatch<'_>,
) -> Result<ScheduleDto, ScheduleOpError> {
    let existing = load_owned_schedule(state, agent_id, session_id, id)?;

    let new_cron = patch.cron_expr.unwrap_or(&existing.cron_expr).to_string();
    let new_tz = patch.timezone.unwrap_or(&existing.timezone).to_string();
    let new_message = patch.message.unwrap_or(&existing.message).to_string();
    let new_enabled = patch.enabled.unwrap_or(existing.enabled);

    validate_schedule(&new_cron, &new_tz).map_err(|e| {
        ScheduleOpError::BadRequest(format!(
            "スケジュール式または timezone が不正です（{e}）。cron は 5 フィールド（例: 0 7 * * *）、周期は @every 3h の形式、timezone は Asia/Tokyo のような IANA 名で指定してください。"
        ))
    })?;
    if new_message.trim().is_empty() {
        return Err(ScheduleOpError::BadRequest(
            "message は空にできません（発火時にエージェントへ渡す指示文を書いてください）。"
                .to_string(),
        ));
    }

    let (anchor_at, last_fired_at) =
        next_anchor_and_last_fired(&existing, &new_cron, &new_tz, new_enabled);

    let row = AgentScheduleRow {
        id: Some(id),
        agent_id: existing.agent_id.clone(),
        session_id: existing.session_id.clone(),
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
            .map_err(|e| ScheduleOpError::Internal(e.to_string()))?;
    }
    state.scheduler_wake.notify_one();

    let saved = {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::get_agent_schedule(&conn, id)
            .map_err(|e| ScheduleOpError::Internal(e.to_string()))?
            .ok_or_else(|| ScheduleOpError::Internal("更新後の行が見つかりません".to_string()))?
    };
    Ok(ScheduleDto::from_row(saved))
}

/// 自分のスケジュールを **id 指定で**削除する共有コア（ツール `delete_my_schedule` 用）。
///
/// `agent_id`＋`session_id` の所属チェックを通った行だけを削除する。成功後 `scheduler_wake`（#437）。
pub(crate) fn delete_schedule_core(
    state: &AppState,
    agent_id: &str,
    session_id: &str,
    id: i64,
) -> Result<(), ScheduleOpError> {
    // 所属チェック（存在しない／他人のものは同じ文言で拒否）。削除できるのは自分のこのセッションの行だけ。
    let _ = load_owned_schedule(state, agent_id, session_id, id)?;
    {
        let conn = state.db.lock().unwrap();
        opencrab_db::queries::delete_agent_schedule(&conn, id)
            .map_err(|e| ScheduleOpError::Internal(e.to_string()))?;
    }
    state.scheduler_wake.notify_one();
    Ok(())
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
    if state
        .timed_fire_router
        .resolve_target(&new_session_id, &existing.agent_id)
        .is_none()
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    if new_message.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // アンカーの向き（§4.4）。dashboard PATCH とエージェント向け update で同じ規則を共有する。
    let (anchor_at, last_fired_at) =
        next_anchor_and_last_fired(&existing, &new_cron, &new_tz, new_enabled);

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

    // #654: nostr セッションで作成する。resolve_target は NostrFire descriptor（nostr feature）が
    // 要る（#651）。off では作成が 400/fail-closed になり検証対象の挙動が存在しないので同じ cfg で囲む。
    #[cfg(feature = "nostr")]
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

    // #654: nostr セッションで作成→更新→削除する。NostrFire（nostr feature）が要る（#651）。
    #[cfg(feature = "nostr")]
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

    // #654: nostr セッションで作成→cron 更新する。NostrFire（nostr feature）が要る（#651）。
    #[cfg(feature = "nostr")]
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

    // ---- 冪等性（同じ内容の再登録で二重発火しない・マージ前修正） ----

    // #654: nostr セッションで作成する。create_schedule_core の resolve_target は NostrFire
    // （nostr feature）が要る（#651）。off では .expect が落ちるので同じ cfg で囲む。
    #[cfg(feature = "nostr")]
    #[test]
    fn create_is_idempotent_on_same_content() {
        let state = state_with_db();
        let s = nostr_session();
        let a = create_schedule_core(&state, AGENT, &s, "@every 3h", "Asia/Tokyo", "巡回", true)
            .expect("1st ok");
        let b = create_schedule_core(&state, AGENT, &s, "@every 3h", "Asia/Tokyo", "巡回", true)
            .expect("2nd ok");
        assert_eq!(a.id, b.id, "同一内容の再登録は同じ id を返す（冪等）");
        // 行は 1 本だけ。
        let conn = state.db.lock().unwrap();
        let rows = opencrab_db::queries::list_agent_schedules(&conn, AGENT).unwrap();
        assert_eq!(rows.len(), 1, "同一内容は 1 本のまま（二重発火しない）");
    }

    // #654: nostr セッションで作成する。NostrFire（nostr feature）が要る（#651）。
    #[cfg(feature = "nostr")]
    #[test]
    fn same_cron_different_message_is_two_schedules() {
        let state = state_with_db();
        let s = nostr_session();
        let a = create_schedule_core(&state, AGENT, &s, "0 7 * * *", "Asia/Tokyo", "まとめ", true)
            .expect("a");
        let b = create_schedule_core(&state, AGENT, &s, "0 7 * * *", "Asia/Tokyo", "巡回", true)
            .expect("b");
        assert_ne!(a.id, b.id, "同じ cron でも message が違えば別スケジュール");
        let conn = state.db.lock().unwrap();
        let rows = opencrab_db::queries::list_agent_schedules(&conn, AGENT).unwrap();
        assert_eq!(rows.len(), 2, "cron だけで dedup しない");
    }

    /// 既に有効な同一内容の再登録では位相（anchor/last_fired）を保存する（冪等 = 時刻も動かさない）。
    // #654: nostr セッションで作成する。NostrFire（nostr feature）が要る（#651）。
    #[cfg(feature = "nostr")]
    #[test]
    fn idempotent_reregister_preserves_phase() {
        let state = state_with_db();
        let s = nostr_session();
        let a = create_schedule_core(&state, AGENT, &s, "@every 3h", "Asia/Tokyo", "巡回", true)
            .expect("a");
        // last_fired を刻んでおく（発火済みを模す）。
        {
            let conn = state.db.lock().unwrap();
            opencrab_db::queries::set_agent_schedule_last_fired(
                &conn,
                a.id,
                "2026-08-09T07:00:00Z",
            )
            .unwrap();
        }
        let b = create_schedule_core(&state, AGENT, &s, "@every 3h", "Asia/Tokyo", "巡回", true)
            .expect("b");
        assert_eq!(a.id, b.id);
        assert_eq!(
            b.last_fired_at.as_deref(),
            Some("2026-08-09T07:00:00Z"),
            "有効な同一内容の再登録は位相を保存する（次回発火が動かない＝冪等）"
        );
    }
}
