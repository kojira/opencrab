//! web gateway の HTTP エンドポイント（#154 第1スライス）。
//!
//! - `POST /api/agents/{id}/web/send` — ダッシュボードからの inbound。
//! - `GET  /api/agents/{id}/web/stream?conversation={cid}` — SSE 購読。
//!
//! session_id 規約・直列化・SSE 配送・非ブロック dispatch の実体は
//! `crate::web_gateway` に置き、ここは HTTP 境界（抽出・認可・DB 記録）だけを持つ。

use std::convert::Infallible;

use axum::{
    extract::{Path, Query, State},
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::Stream;
use serde::Deserialize;

use crate::web_gateway::{run_and_deliver, web_session_id};
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct SendWebMessageRequest {
    pub conversation_id: String,
    pub content: String,
    /// 認可判定用のユーザ ID（省略時は匿名 web ユーザ）。
    #[serde(default)]
    pub user_id: Option<String>,
}

/// POST /api/agents/{id}/web/send — inbound メッセージを受けてエージェントを実行する。
///
/// per-session 直列化の下で `run_and_deliver` を実行し、直接応答を body で返しつつ
/// SSE へも push する。subtask 完了 resume の応答は SSE のみで配送される。
pub async fn send_web_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendWebMessageRequest>,
) -> Json<serde_json::Value> {
    let user_id = req
        .user_id
        .clone()
        .unwrap_or_else(|| "web-user".to_string());
    let session_id = web_session_id(&id, &req.conversation_id);

    // 1. 認可: 既存 REST（agents_messages）に倣い trusted_users から caller を導出する。
    let caller = {
        let conn = state.db.lock().unwrap();
        match opencrab_db::queries::get_trusted_user(&conn, &user_id, &id) {
            Some(u) if u.permission == "co_agent" => opencrab_actions::CallerIdentity::CoAgent {
                agent_id: user_id.clone(),
            },
            Some(_) => opencrab_actions::CallerIdentity::TrustedUser,
            None => {
                let cfg = opencrab_db::queries::get_agent_discord_config(&conn, &id);
                if let Ok(Some(c)) = cfg {
                    if c.owner_discord_id == user_id {
                        opencrab_actions::CallerIdentity::Owner
                    } else {
                        opencrab_actions::CallerIdentity::Agent
                    }
                } else {
                    opencrab_actions::CallerIdentity::Agent
                }
            }
        }
    };
    let caller_type = match &caller {
        opencrab_actions::CallerIdentity::CoAgent { .. } => "co_agent",
        opencrab_actions::CallerIdentity::TrustedUser => "trusted_user",
        opencrab_actions::CallerIdentity::Owner => "owner",
        _ => "agent",
    };

    // 2. セッションを用意する（無ければ作成）。
    {
        let conn = state.db.lock().unwrap();
        let existing = opencrab_db::queries::get_session(&conn, &session_id)
            .ok()
            .flatten();
        if existing.is_none() {
            let session = opencrab_db::queries::SessionRow {
                id: session_id.clone(),
                mode: "autonomous".to_string(),
                theme: "web_conversation".to_string(),
                phase: "divergent".to_string(),
                turn_number: 0,
                status: "active".to_string(),
                participant_ids_json: serde_json::json!([&id]).to_string(),
                facilitator_id: None,
                done_count: 0,
                max_turns: None,
                metadata_json: None,
            };
            if let Err(e) = opencrab_db::queries::insert_session(&conn, &session) {
                return Json(
                    serde_json::json!({"error": format!("Failed to create session: {e}")}),
                );
            }
        }
    }

    // 3. ユーザ発話を DB へ記録する（run_and_deliver は DB から会話を再構築する）。
    {
        let conn = state.db.lock().unwrap();
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: id.clone(),
            session_id: session_id.clone(),
            log_type: "speech".to_string(),
            content: req.content.clone(),
            speaker_id: Some(user_id.clone()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
            return Json(serde_json::json!({"error": format!("Failed to log message: {e}")}));
        }
    }

    // 4. LLM プロバイダの可用性チェック。
    if state.llm_router.get().provider_names().is_empty() {
        return Json(serde_json::json!({
            "session_id": session_id,
            "caller_type": caller_type,
            "response": null,
            "error": "No LLM providers available",
        }));
    }

    // 5. per-session 直列化の下で実行し、直接応答を返す（SSE へも push 済み）。
    let gateway = state.web_gateway.clone();
    let response = gateway
        .run_serialized(
            &session_id,
            run_and_deliver(&state, &gateway, &id, &session_id, caller, None, "direct"),
        )
        .await;

    Json(serde_json::json!({
        "session_id": session_id,
        "caller_type": caller_type,
        "response": response,
    }))
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    pub conversation: String,
}

/// GET /api/agents/{id}/web/stream?conversation={cid} — SSE でエージェント発話を購読する。
///
/// inbound への直接応答と subtask 完了 resume の応答の双方が push される。
/// 未接続時に発話が失われないよう、応答は DB（session_logs）にも保存されている。
pub async fn web_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let session_id = web_session_id(&id, &q.conversation);
    let rx = state.web_gateway.subscribe(&session_id);

    // broadcast::Receiver を SSE ストリームへ変換する（tokio-stream 依存を避け futures で）。
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(payload) => {
                    return Some((Ok(Event::default().data(payload)), rx));
                }
                // 遅い購読者でバックログが溢れた: 落ちた分は DB 側で辿れる。継続する。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                // 送信側が全て drop された: ストリーム終了。
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}
