//! 管理面（ダッシュボード）の読み取り API + agents/owner 書き系復元。
//!
//! ここは配送と整形だけを担う（AGREED §2.9）。データの判断・クエリは store 側に置き、
//! この面は store の型付き読み取り／owner コマンドを呼んで、旧ダッシュボードの JSON 形へ写すだけ。
//!
//! oc2 が概念を置き換えたもの（agent→subject、session→place、会話ログ→events/turn_records、
//! memory→memories）は store の新テーブルへ向き先を差し替える。レスポンス形は旧のまま＝フロント無改変。
//! oc2 に対応物が無い / まだデータが入らないものは、**偽の空応答を返さず** 501 で明示的に未実装を返す。

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, MethodRouter},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use opencrab_db::{queries, Db};
use opencrab_port::{EventKind, SubjectKind};
use opencrab_store::Store;

use crate::schedule_cron::schedule_next_fire_at;

#[derive(Clone)]
pub struct AdminState {
    /// oc2 の新テーブル（subjects/places/events/turn_records/memories）を読む観測者。
    pub store: Arc<Store>,
    /// 本体 DB スキーマ（正本・AGREED §2.11）の旧テーブル。D25 は voice_config_override へ書く。
    pub db: Arc<Db>,
    /// 文脈予算 = context_window × compaction_ratio。model-pricing 応答に載せる server-global 値。
    pub compaction_ratio: f64,
}

/// store のエラーは配送側の 500（読み取りの失敗＝内部エラー）。中身は握り潰さず detail に出す。
fn store_err(e: rusqlite::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "store_error", "detail": e.to_string() })),
    )
}

/// 旧テーブルの読み取り結果を配送する。**テーブルが無い＝統合 DB へ未移行**を 501 で明示的に
/// 区別する（偽の空配列を返さない・migration 側の責務）。それ以外の失敗は 500。
/// opencrab-db の queries は `anyhow::Result` を返すので、エラー型は Display で受ける
/// （rusqlite の "no such table" メッセージは anyhow の Display にそのまま乗る）。
fn db_read<T, E: std::fmt::Display>(r: Result<T, E>) -> ApiResult<T> {
    r.map_err(|e| {
        let msg = e.to_string();
        // テーブルも列も、正本スキーマ（本体 DB・§2.11）へ未移行という同じ種類の欠落。
        // どちらも偽の空を返さず 501 で明示する（列欠落は旧スナップショットで実際に起きた）。
        if msg.contains("no such table") || msg.contains("no such column") {
            (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({
                    "error": "unimplemented",
                    "detail": format!("正本スキーマ（本体 DB）へ未移行です（migration 待ち）: {msg}"),
                })),
            )
        } else {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "db_error", "detail": msg })),
            )
        }
    })
}

/// opencrab-db の接続取得失敗（poison 等）は配送側の 500。
fn db_lock_err(e: opencrab_db::DbError) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "db_lock_error", "detail": e.to_string() })),
    )
}

fn bad_id() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(
            json!({ "error": "bad_id", "detail": "id は整数（subject/place の内部 ID）である必要があります" }),
        ),
    )
}

pub(crate) type ApiResult<T> = Result<T, (StatusCode, Json<serde_json::Value>)>;

/// ナノ秒エポックを RFC3339 文字列へ（events.created_at は ns・store の観測）。0/変換不能は空文字。
fn iso_from_nanos(ts: i64) -> String {
    let secs = ts.div_euclid(1_000_000_000);
    let nanos = ts.rem_euclid(1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

// ---- 旧ダッシュボードのレスポンス形（フロント無改変のため列と型を保つ） ----

#[derive(Serialize)]
struct AgentSummary {
    id: String,
    name: String,
    // oc2 に persona_name（人格テンプレート名）の概念が無いので null（name で埋めない・偽装しない）。
    persona_name: Option<String>,
    image_url: Option<String>,
    status: String,
    // oc2 に skills が無いので「0 件」ではなく「測れない」= null（#766 の usage 0 埋め廃止と同原則）。
    skill_count: Option<i32>,
    session_count: i32,
}

#[derive(Serialize)]
struct AgentDetail {
    id: String,
    name: String,
    job_title: Option<String>,
    organization: Option<String>,
    image_url: Option<String>,
    // oc2 に persona_name の概念が無いので null。
    persona_name: Option<String>,
    personality: Option<String>,
    instructions: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    web_search: Option<bool>,
    metadata_json: Option<String>,
}

#[derive(Serialize)]
struct SessionRow {
    id: String,
    // oc2 の place に mode（旧セッションの議論形式）の概念が無いので null。
    mode: Option<String>,
    theme: String,
    phase: String,
    turn_number: i64,
    status: String,
    participant_ids_json: String,
    facilitator_id: Option<String>,
    // oc2 に done_count の概念が無いので null（フロント未使用）。
    done_count: Option<i64>,
    max_turns: Option<i64>,
    metadata_json: Option<String>,
}

#[derive(Serialize)]
struct SessionLogRow {
    id: Option<i64>,
    agent_id: String,
    session_id: String,
    log_type: String,
    content: String,
    speaker_id: Option<String>,
    turn_number: Option<i64>,
    metadata_json: Option<String>,
    created_at: Option<String>,
}

#[derive(Serialize)]
struct CuratedMemoryDto {
    id: String,
    agent_id: String,
    // oc2 の memories にカテゴリの概念が無いので null（"memory" で埋めない・偽装しない）。
    category: Option<String>,
    content: String,
}

// ---- リダイレクト実装（oc2 store の新テーブル → 旧 JSON 形） ----

/// GET /api/agents — subjects（kind=agent）を旧 AgentSummary へ写す。
/// session_count は「その主体が membership を持つ場の数」。skill_count は oc2 に概念が無いので 0。
async fn list_agents(State(st): State<AdminState>) -> ApiResult<Json<Vec<AgentSummary>>> {
    let subjects = st.store.all_subjects().map_err(store_err)?;
    let places = st.store.all_places().map_err(store_err)?;
    let mut out = Vec::new();
    for s in subjects
        .into_iter()
        .filter(|s| s.kind == SubjectKind::Agent)
    {
        let mut session_count = 0i32;
        for p in &places {
            if st
                .store
                .get_membership(p.id, s.id)
                .map_err(store_err)?
                .is_some()
            {
                session_count += 1;
            }
        }
        out.push(AgentSummary {
            id: s.id.to_string(),
            name: s.name,
            persona_name: None,
            image_url: None,
            status: "idle".to_string(),
            skill_count: None,
            session_count,
        });
    }
    Ok(Json(out))
}

/// GET /api/agents/{id} — subject + profile を旧 AgentDetail の平坦形へ。無ければ null（旧挙動）。
async fn get_agent(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let sid: i64 = id.parse().map_err(|_| bad_id())?;
    let view = st.store.subject_dashboard_view(sid).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "store_error", "detail": e.to_string() })),
        )
    })?;
    let Some(view) = view else {
        return Ok(Json(serde_json::Value::Null));
    };
    let detail = AgentDetail {
        id: view.id.to_string(),
        name: view.name,
        job_title: None,
        organization: None,
        image_url: None,
        persona_name: view.persona_name,
        personality: view.personality,
        instructions: view.instructions,
        model: view.model,
        reasoning_effort: None,
        web_search: None,
        metadata_json: None,
    };
    Ok(Json(serde_json::to_value(detail).unwrap()))
}

fn place_to_session_row(st: &AdminState, place_id: i64) -> ApiResult<SessionRow> {
    let Some(p) = st.store.get_place(place_id).map_err(store_err)? else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "not_found", "detail": "place が存在しません" })),
        ));
    };
    let members = st.store.members(p.id).map_err(store_err)?;
    let participant_ids: Vec<String> = members.iter().map(|m| m.subject.to_string()).collect();
    let turn_number = st.store.turn_records(p.id).map_err(store_err)?.len() as i64;
    Ok(SessionRow {
        id: p.id.to_string(),
        mode: None,
        theme: p.address.clone().unwrap_or_default(),
        phase: String::new(),
        turn_number,
        // フロント（Sessions.tsx フィルタ・Home.tsx 件数）は active/completed 語彙。
        // 場の開閉（closed_at）を旧語彙へ写す: 開いている=active、閉じている=completed。
        status: if p.closed_at.is_some() {
            "completed".to_string()
        } else {
            "active".to_string()
        },
        participant_ids_json: serde_json::to_string(&participant_ids).unwrap(),
        facilitator_id: None,
        done_count: None,
        max_turns: None,
        metadata_json: Some(p.policy_json),
    })
}

/// GET /api/sessions — 全 place（開・閉とも）を旧 SessionRow 形で。フロントが agent_ids/participant_count を導出。
async fn list_sessions(State(st): State<AdminState>) -> ApiResult<Json<Vec<SessionRow>>> {
    let places = st.store.all_places().map_err(store_err)?;
    let mut out = Vec::with_capacity(places.len());
    for p in places {
        out.push(place_to_session_row(&st, p.id)?);
    }
    Ok(Json(out))
}

/// GET /api/sessions/{id} — 単一 place を旧 SessionRow 形で。
async fn get_session(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionRow>> {
    let pid: i64 = id.parse().map_err(|_| bad_id())?;
    Ok(Json(place_to_session_row(&st, pid)?))
}

/// GET /api/sessions/{id}/logs — place の events を会話ログ（旧 SessionLogRow）へ写す。
/// これが SessionDetail の会話履歴表示。log_type は出来事の種別（said/spoke/...）。
async fn list_session_logs(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<SessionLogRow>>> {
    let pid: i64 = id.parse().map_err(|_| bad_id())?;
    let last = st.store.latest_seq(pid).map_err(store_err)?;
    let events = st.store.read_range(pid, 0, last).map_err(store_err)?;
    let logs = events
        .into_iter()
        .map(|ev| {
            let speaker = ev
                .author_subject
                .map(|s| s.to_string())
                .or_else(|| ev.author_external.clone());
            SessionLogRow {
                id: Some(ev.seq),
                agent_id: ev.author_subject.map(|s| s.to_string()).unwrap_or_default(),
                session_id: pid.to_string(),
                log_type: ev.kind.as_str().to_string(),
                content: content_text(&ev.kind, ev.content.text.clone()),
                speaker_id: speaker,
                turn_number: None,
                metadata_json: None,
                created_at: Some(iso_from_nanos(ev.created_at)),
            }
        })
        .collect();
    Ok(Json(logs))
}

/// 反応など text の無い出来事は symbol も本文が空になり得る。空でも偽装せずそのまま出す。
fn content_text(_kind: &EventKind, text: Option<String>) -> String {
    text.unwrap_or_default()
}

/// GET /api/agents/{id}/memory/curated — その主体の memories を {items,total} 封筒で。
async fn list_curated_memory(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let sid: i64 = id.parse().map_err(|_| bad_id())?;
    let memories = st.store.memories_newest_first(sid).map_err(store_err)?;
    let items: Vec<CuratedMemoryDto> = memories
        .into_iter()
        .map(|m| CuratedMemoryDto {
            id: m.id.to_string(),
            agent_id: sid.to_string(),
            category: None,
            content: m.body,
        })
        .collect();
    let total = items.len();
    Ok(Json(json!({ "items": items, "total": total })))
}

// ---- 旧テーブルの読み取り（本体 DB スキーマが正本・AGREED §2.11）。旧 crates/server の
//      ハンドラを移植。opencrab-db の queries をそのまま呼び、旧 JSON 形で返す。統合 DB へ
//      未移行のテーブルは db_read が 501 で明示する（現在の oc2 DB には旧テーブルが無い）。 ----

/// path の {id}（＝/api/agents が返す subject の内部 int id）を、旧テーブルのキーである
/// **旧 agent_id（UUID/スラッグ）へ実際に結合して解決する**。ID 空間が違うまま旧表を引くと、
/// マイグレーション後に「データがあるのに空配列」＝偽の空になるため（レビュー blocker 1）。
///
/// §2.11 で本体 DB の旧 `agents` 表が正本なので、それを読み subject の表示名で突き合わせる
/// （converter は agents.name を subject の表示名へ写すので name が結合キー）。旧 `agents` 表が
/// まだ無い（未移行）・subject が無い・対応する旧 agent が無い場合は `None` を返し、呼び手が 501
/// で明示する（偽の空を返さない）。統合スキーマに subjects.public_id（旧 UUID）が入ったら、
/// name 結合を public_id 結合へ差し替える（issue-later）。
pub(crate) fn resolve_legacy_agent_id(
    st: &AdminState,
    subject_id: i64,
) -> ApiResult<Option<String>> {
    let Some(subject) = st.store.get_subject(subject_id).map_err(store_err)? else {
        return Ok(None);
    };
    let conn = st.db.lock().map_err(db_lock_err)?;
    let found: rusqlite::Result<String> = conn.query_row(
        "SELECT agent_id FROM agents WHERE name = ?1 LIMIT 1",
        rusqlite::params![subject.name],
        |r| r.get(0),
    );
    match found {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        // 旧 `agents` 表が未移行（no such table）も「解決不能」として None（呼び手が 501）。
        Err(e) if e.to_string().contains("no such table") => Ok(None),
        Err(e) => Err(store_err(e)),
    }
}

/// subject→旧 agent_id を解決できないときの明示 501（偽の空配列を返さない）。
pub(crate) fn unresolved_agent() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "unimplemented",
            "detail": "subject に対応する旧 agent_id を解決できません（本体 DB の agents 表が未移行、または対応行なし）。統合 DB のマイグレーション後に解決されます。",
        })),
    )
}

/// GET /api/agents/{id}/allowed-commands — 許可コマンドの一覧（DB 行のみ・#300）。
async fn list_allowed_commands(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    let sid: i64 = id.parse().map_err(|_| bad_id())?;
    let Some(agent_id) = resolve_legacy_agent_id(&st, sid)? else {
        return Err(unresolved_agent());
    };
    let conn = st.db.lock().map_err(db_lock_err)?;
    let commands = db_read(queries::list_agent_allowed_commands(&conn, &agent_id))?;
    Ok(Json(
        commands
            .into_iter()
            .map(|c| json!({ "command": c }))
            .collect(),
    ))
}

/// 旧 TrustedUserDto（`opencrab_db::queries::TrustedUserRow` は Serialize でないため、旧 server と
/// 同じ形へ写す）。permission は列挙型の serde 表現がそのまま出る（キー据え置き・値ケバブケース・#234）。
#[derive(Serialize)]
struct TrustedUserDto {
    id: String,
    user_id: String,
    agent_id: String,
    permission: queries::TrustedUserPermission,
    created_by: String,
    created_at: String,
    display_name: String,
    platform: String,
}

/// GET /api/agents/{id}/trusted-users — 信頼済みユーザー一覧（全経路・認可には使わない）。
async fn list_trusted_users(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<TrustedUserDto>>> {
    let sid: i64 = id.parse().map_err(|_| bad_id())?;
    let Some(agent_id) = resolve_legacy_agent_id(&st, sid)? else {
        return Err(unresolved_agent());
    };
    let conn = st.db.lock().map_err(db_lock_err)?;
    let rows = db_read(queries::list_trusted_users(&conn, &agent_id))?;
    let dtos = rows
        .into_iter()
        .map(|r| TrustedUserDto {
            id: r.id,
            user_id: r.user_id,
            agent_id: r.agent_id,
            permission: r.permission,
            created_by: r.created_by,
            created_at: r.created_at,
            display_name: r.display_name,
            platform: r.platform,
        })
        .collect();
    Ok(Json(dtos))
}

/// GET /api/llm/model-pricing — モデル単価・文脈長の一覧＋ server-global の compaction_ratio。
async fn list_model_pricing(State(st): State<AdminState>) -> ApiResult<Json<serde_json::Value>> {
    let conn = st.db.lock().map_err(db_lock_err)?;
    let rows = db_read(queries::list_model_pricing(&conn))?;
    Ok(Json(json!({
        "models": rows,
        "compaction_ratio": st.compaction_ratio,
    })))
}

/// スケジュールの旧 DTO（旧 crates/server::api::schedules から移植）。next_fire_at は照会時算出。
#[derive(Serialize)]
struct ScheduleDto {
    id: i64,
    agent_id: String,
    session_id: String,
    cron_expr: String,
    timezone: String,
    message: String,
    enabled: bool,
    anchor_at: Option<String>,
    last_fired_at: Option<String>,
    next_fire_at: Option<String>,
    gated: bool,
    gated_reason: Option<String>,
}

fn parse_wall_clock(s: &Option<String>) -> Option<chrono::DateTime<chrono::Utc>> {
    let s = s.as_ref()?;
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

impl ScheduleDto {
    fn from_row(row: queries::AgentScheduleRow) -> Self {
        // 照会時算出（真実は再計算・キャッシュしない）。旧 crates/server と同一ロジック。
        let next_fire_at = schedule_next_fire_at(
            &row.cron_expr,
            &row.timezone,
            parse_wall_clock(&row.anchor_at),
            parse_wall_clock(&row.last_fired_at),
        )
        .ok()
        .flatten()
        .map(|t| t.to_rfc3339());
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

/// GET /api/agents/{id}/schedules — 定時実行スケジュール一覧（旧 {agent_id,schedules,count} 封筒）。
async fn list_schedules(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let sid: i64 = id.parse().map_err(|_| bad_id())?;
    let Some(agent_id) = resolve_legacy_agent_id(&st, sid)? else {
        return Err(unresolved_agent());
    };
    let conn = st.db.lock().map_err(db_lock_err)?;
    let rows = db_read(queries::list_agent_schedules(&conn, &agent_id))?;
    let schedules: Vec<ScheduleDto> = rows.into_iter().map(ScheduleDto::from_row).collect();
    let count = schedules.len();
    Ok(Json(json!({
        "agent_id": agent_id,
        "schedules": schedules,
        "count": count,
    })))
}

// ---- llm_logs / tool_logs（#772 A・本体封筒。writer は agent_id に subject 十進を書く） ----

/// 本体 API `/llm-logs` の 1 行。フロント（AgentLlmLogs.tsx）が読む列だけを載せる。
#[derive(Serialize)]
struct LlmLogDto {
    id: String,
    agent_id: String,
    session_id: Option<String>,
    model: Option<String>,
    prompt: String,
    response: String,
    tool_calls: Option<String>,
    latency_ms: Option<i64>,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    total_tokens: Option<i64>,
    error_code: Option<String>,
    error_body: Option<String>,
    requested_at: Option<String>,
    trigger_message_id: Option<String>,
    is_bot_iteration: bool,
    cache_read_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    created_at: String,
}

impl From<opencrab_store::LlmLogRow> for LlmLogDto {
    fn from(r: opencrab_store::LlmLogRow) -> Self {
        Self {
            id: r.id,
            agent_id: r.agent_id,
            session_id: r.session_id,
            model: r.model,
            prompt: r.prompt,
            response: r.response,
            tool_calls: r.tool_calls,
            latency_ms: r.latency_ms,
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            total_tokens: r.total_tokens,
            error_code: r.error_code,
            error_body: r.error_body,
            requested_at: r.requested_at,
            trigger_message_id: r.trigger_message_id,
            is_bot_iteration: r.is_bot_iteration,
            cache_read_tokens: r.cache_read_tokens,
            cache_creation_tokens: r.cache_creation_tokens,
            created_at: r.created_at,
        }
    }
}

/// 本体 API `/llm-logs/stats` の日次行。
#[derive(Serialize)]
struct LlmLogStatDto {
    date: String,
    count: i64,
    total_tokens: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    avg_latency_ms: f64,
    error_count: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
}

impl From<opencrab_store::LlmLogStatRow> for LlmLogStatDto {
    fn from(r: opencrab_store::LlmLogStatRow) -> Self {
        Self {
            date: r.date,
            count: r.count,
            total_tokens: r.total_tokens,
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            avg_latency_ms: r.avg_latency_ms,
            error_count: r.error_count,
            cache_read_tokens: r.cache_read_tokens,
            cache_creation_tokens: r.cache_creation_tokens,
        }
    }
}

/// tool_logs の 1 行（#787）。フロントページは未着手。観測に必要な列だけ。
#[derive(Serialize)]
struct ToolLogDto {
    id: i64,
    agent_id: String,
    session_id: Option<String>,
    tool_name: String,
    args_json: String,
    outcome: String,
    result_text: String,
    started_at: Option<String>,
    created_at: String,
    latency_ms: Option<i64>,
    turn_record_id: Option<i64>,
    activity_id: Option<i64>,
    iteration: Option<i64>,
}

impl From<opencrab_store::ToolLogRow> for ToolLogDto {
    fn from(r: opencrab_store::ToolLogRow) -> Self {
        Self {
            id: r.id,
            agent_id: r.agent_id,
            session_id: r.session_id,
            tool_name: r.tool_name,
            args_json: r.args_json,
            outcome: r.outcome,
            result_text: r.result_text,
            started_at: r.started_at,
            created_at: r.created_at,
            latency_ms: r.latency_ms,
            turn_record_id: r.turn_record_id,
            activity_id: r.activity_id,
            iteration: r.iteration,
        }
    }
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct DaysQuery {
    days: Option<i64>,
}

fn positive_or(n: Option<i64>, default: i64) -> i64 {
    n.filter(|v| *v > 0).unwrap_or(default)
}

/// GET /api/agents/{id}/llm-logs — 本体封筒（配列）。agent_id は path の subject 十進。
async fn list_llm_logs(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<Vec<LlmLogDto>>> {
    let sid: i64 = id.parse().map_err(|_| bad_id())?;
    let rows = db_read(
        st.store
            .list_llm_logs(&sid.to_string(), positive_or(q.limit, 20)),
    )?;
    Ok(Json(rows.into_iter().map(LlmLogDto::from).collect()))
}

/// GET /api/agents/{id}/llm-logs/stats — 本体封筒（日次配列・既定 30 日）。
async fn llm_logs_stats(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Query(q): Query<DaysQuery>,
) -> ApiResult<Json<Vec<LlmLogStatDto>>> {
    let sid: i64 = id.parse().map_err(|_| bad_id())?;
    let rows = db_read(
        st.store
            .llm_logs_stats(&sid.to_string(), positive_or(q.days, 30)),
    )?;
    Ok(Json(rows.into_iter().map(LlmLogStatDto::from).collect()))
}

/// GET /api/agents/{id}/tool-logs — tool_logs を新しい順で返す（本体 llm-logs と同型の配列）。
async fn list_tool_logs(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<Vec<ToolLogDto>>> {
    let sid: i64 = id.parse().map_err(|_| bad_id())?;
    let rows = db_read(
        st.store
            .list_tool_logs(&sid.to_string(), positive_or(q.limit, 20)),
    )?;
    Ok(Json(rows.into_iter().map(ToolLogDto::from).collect()))
}

// ---- 未実装（偽装しない・501 で理由を明示） ----

/// GET だけを載せ、理由文つきの 501 を返す MethodRouter を作る。
/// oc2 に概念が無い / 旧テーブルが統合 DB へ未移行 / runtime 依存で Phase 1 の範囲外、を区別して伝える。
fn unimpl(reason: &'static str) -> MethodRouter<AdminState> {
    get(move || async move {
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "unimplemented", "detail": reason })),
        )
    })
}

async fn health() -> impl IntoResponse {
    "ok"
}

async fn api_health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// 読み系ルート表。旧 create_router の GET 面を移植し、実データのあるものはリダイレクト、
/// それ以外は未実装を明示。owner ID（discord/nostr）だけ本体 wire の書き系を復元する。
/// それ以外の未載メソッドは axum が 405 を返す（偽の成功を作らない）。
pub fn create_router(state: AdminState) -> Router {
    Router::new()
        // health
        .route("/health", get(health))
        .route("/api/health", get(api_health))
        // --- 実データのあるリダイレクト（oc2 store の新テーブル） ---
        .route("/api/agents", get(list_agents))
        .route("/api/agents/{id}", get(get_agent))
        .merge(crate::agent_routes::agent_write_routes())
        .merge(crate::skill_routes::skill_write_routes())
        .merge(crate::memory_routes::memory_write_routes())
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}", get(get_session))
        .route("/api/sessions/{id}/logs", get(list_session_logs))
        .merge(crate::session_routes::session_write_routes())
        .route("/api/agents/{id}/memory/curated", get(list_curated_memory))
        // --- 旧テーブルの読み取り（本体 DB スキーマが正本・§2.11）。opencrab-db 経由で配線。
        //     現 oc2 DB には旧テーブルが無いので db_read が 501（未移行）を返す＝偽装しない。 ---
        .route("/api/agents/{id}/schedules", get(list_schedules))
        .route("/api/agents/{id}/trusted-users", get(list_trusted_users))
        .route(
            "/api/agents/{id}/allowed-commands",
            get(list_allowed_commands),
        )
        .route("/api/llm/model-pricing", get(list_model_pricing))
        // --- llm_logs / tool_logs（#772 A・store の読み取り。表が無いときだけ 501） ---
        .route("/api/agents/{id}/llm-logs", get(list_llm_logs))
        .route("/api/agents/{id}/llm-logs/stats", get(llm_logs_stats))
        .route("/api/agents/{id}/tool-logs", get(list_tool_logs))
        // --- まだ未配線の旧テーブル系（統合 DB / 後続で配線） ---
        .route(
            "/api/agents/{id}/sleep-logs",
            unimpl("sleep-logs: 旧テーブル未移行（統合 DB 待ち）"),
        )
        // --- skills unused は本スライス対象外（GET list/CRUD は skill_write_routes） ---
        .route(
            "/api/agents/{id}/skills/unused",
            unimpl("skills unused: unrestored subroute"),
        )
        .route(
            "/api/agents/{id}/analytics",
            unimpl("analytics: 記録系が oc2 未実装"),
        )
        .route(
            "/api/agents/{id}/analytics/detail",
            unimpl("analytics: 記録系が oc2 未実装"),
        )
        .route(
            "/api/agents/{id}/co-agents",
            unimpl("co-agents: oc2 に概念なし"),
        )
        // --- memory index GET status/tree は本スライス対象外（WRITE は memory_write_routes） ---
        .route(
            "/api/agents/{id}/memory/index/tree",
            unimpl("memory index tree: unrestored subroute"),
        )
        .route(
            "/api/agents/{id}/daily-log-index/status",
            unimpl("daily-log-index: oc2 未実装"),
        )
        // --- runtime 依存で Phase 1（読み取り）の範囲外 ---
        .route(
            "/api/setup/status",
            unimpl("setup status: runtime 集約が範囲外"),
        )
        .route(
            "/api/llm/model-choices",
            unimpl("model-choices: LLM router（runtime）が範囲外"),
        )
        .route(
            "/api/llm/providers",
            unimpl("providers: LLM router（runtime）が範囲外"),
        )
        .route(
            "/api/llm/codex/diagnostics",
            unimpl("diagnostics: runtime が範囲外"),
        )
        .route(
            "/api/llm/cursor/diagnostics",
            unimpl("diagnostics: runtime が範囲外"),
        )
        .route(
            "/api/llm/acp/diagnostics",
            unimpl("diagnostics: runtime が範囲外"),
        )
        .merge(crate::voice_routes::voice_config_routes())
        .route(
            "/api/agents/{id}/workspace",
            unimpl("workspace: ファイル面が範囲外"),
        )
        .route(
            "/api/agents/{id}/mcp",
            unimpl("mcp: 設定・runtime が範囲外"),
        )
        .route(
            "/api/agents/{id}/channel-configs",
            unimpl("channel-configs: oc2 は別系（gate）に再設計・範囲外"),
        )
        .merge(crate::owner_routes::owner_id_routes())
        .route(
            "/api/agents/{id}/nostr-relay",
            unimpl("nostr-relay 設定: 範囲外"),
        )
        .route(
            "/api/agents/{id}/import/sync/status",
            unimpl("import: oc2 未実装"),
        )
        .route(
            "/api/agents/{id}/import/sync/history",
            unimpl("import: oc2 未実装"),
        )
        .route(
            "/api/system/log-level",
            unimpl("system log-level: runtime が範囲外"),
        )
        .with_state(state)
}

#[cfg(test)]
mod api_contract {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use opencrab_port::Standing;
    use opencrab_store::{NewLlmLog, NewToolLog, Store};
    use tower::ServiceExt;

    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn dummy_db() -> Arc<Db> {
        Arc::new(Db::from_connection(
            rusqlite::Connection::open_in_memory().expect("memory db"),
        ))
    }

    fn state_from_store(store: Store) -> AdminState {
        AdminState {
            store: Arc::new(store),
            db: dummy_db(),
            compaction_ratio: 0.5,
        }
    }

    fn seed_logs(store: &Store) -> i64 {
        let agent = store
            .create_subject(
                SubjectKind::Agent,
                "A",
                "persona",
                "engine",
                Standing::Trusted,
                0,
            )
            .expect("subject");
        let ts = now_rfc3339();
        store
            .write_llm_log(&NewLlmLog {
                id: format!("{agent}-1"),
                agent_id: agent.to_string(),
                session_id: Some("3".into()),
                model: Some("scripted".into()),
                prompt: r#"{"model":"scripted","messages":[{"role":"user","content":"hi"}]}"#
                    .into(),
                response: r#"{"content":"yo","tool_calls":[],"finish_reason":"stop","usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}"#.into(),
                tool_calls: None,
                latency_ms: Some(12),
                prompt_tokens: Some(2),
                completion_tokens: Some(1),
                total_tokens: Some(3),
                error_code: None,
                error_body: None,
                requested_at: Some(ts.clone()),
                trigger_message_id: Some("9".into()),
                is_bot_iteration: false,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                created_at: ts.clone(),
                turn_record_id: None,
                iteration: Some(1),
                place_id: Some(3),
                subject_id: Some(agent),
            })
            .expect("llm log");
        store
            .write_tool_log(&NewToolLog {
                agent_id: agent.to_string(),
                session_id: Some("3".into()),
                tool_name: "core-recall".into(),
                args_json: "{}".into(),
                outcome: "done".into(),
                result_text: "ok".into(),
                started_at: Some(ts.clone()),
                created_at: ts,
                latency_ms: Some(4),
                turn_record_id: None,
                activity_id: None,
                iteration: Some(1),
                place_id: Some(3),
                subject_id: Some(agent),
            })
            .expect("tool log");
        agent
    }

    async fn get_json(state: AdminState, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = create_router(state)
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("oneshot");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    /// TEST-DESIGN D22: GET /api/agents/{id}/llm-logs は 200 + 本体封筒。
    #[tokio::test]
    async fn llm_logs_list_is_200_body_envelope() {
        let store = Store::new_in_memory().expect("store");
        let agent = seed_logs(&store);
        let (status, body) = get_json(
            state_from_store(store),
            &format!("/api/agents/{agent}/llm-logs"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let row = body
            .as_array()
            .expect("list envelope")
            .first()
            .expect("one row");
        assert_eq!(row["agent_id"], agent.to_string());
        assert_eq!(row["prompt_tokens"], 2);
        assert_eq!(row["completion_tokens"], 1);
        assert!(row["created_at"].as_str().is_some());
        assert!(row["prompt"].as_str().unwrap().contains("messages"));
    }

    /// TEST-DESIGN D22: GET /api/agents/{id}/llm-logs/stats は 200 + 日次配列。
    #[tokio::test]
    async fn llm_logs_stats_is_200_body_envelope() {
        let store = Store::new_in_memory().expect("store");
        let agent = seed_logs(&store);
        let (status, body) = get_json(
            state_from_store(store),
            &format!("/api/agents/{agent}/llm-logs/stats"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let row = body
            .as_array()
            .expect("stats envelope")
            .first()
            .expect("one day");
        assert_eq!(row["count"], 1);
        assert_eq!(row["prompt_tokens"], 2);
        assert_eq!(row["completion_tokens"], 1);
        assert!(row["date"].as_str().is_some());
    }

    #[tokio::test]
    async fn tool_logs_list_is_200() {
        let store = Store::new_in_memory().expect("store");
        let agent = seed_logs(&store);
        let (status, body) = get_json(
            state_from_store(store),
            &format!("/api/agents/{agent}/tool-logs"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let row = body
            .as_array()
            .expect("tool-logs envelope")
            .first()
            .expect("one row");
        assert_eq!(row["agent_id"], agent.to_string());
        assert_eq!(row["tool_name"], "core-recall");
        assert_eq!(row["outcome"], "done");
        assert_eq!(row["result_text"], "ok");
    }

    #[tokio::test]
    async fn llm_logs_absent_table_is_501() {
        let path = std::env::temp_dir().join(format!(
            "opencrab-admin-llm-logs-absent-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        rusqlite::Connection::open(&path).expect("empty sqlite");
        let store = Store::open_read_only(&path).expect("ro");
        let db = Arc::new(Db::from_connection(
            rusqlite::Connection::open_with_flags(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .expect("ro db"),
        ));
        let (status, body) = get_json(
            AdminState {
                store: Arc::new(store),
                db,
                compaction_ratio: 0.5,
            },
            "/api/agents/1/llm-logs",
        )
        .await;
        let _ = std::fs::remove_file(&path);
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{body}");
        assert_eq!(body["error"], "unimplemented");
    }
}
