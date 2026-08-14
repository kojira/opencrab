use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use opencrab_actions::{
    gateway_kinds, SettleKind, SubtaskCompletionSink, SubtaskRegistry, SubtaskSettled,
};

use crate::process;
use crate::process::AgentNotFound;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct SendAgentMessageRequest {
    pub content: String,
    pub user_id: String,
}

/// REST 経路のセッション ID 接頭辞（`agent-msg-{agent_id}-{user_id}`）。
pub const REST_SESSION_PREFIX: &str = "agent-msg-";

/// REST 経路の完了 sink（#169）。**resume も live push もしない「保存のみ」**。
///
/// 完了本文は `settle_completed` が親セッションログへ永続化済み（RFC §1.3）なので、
/// 取得は `GET /api/sessions/{id}/logs` で足りる。ここで LLM を回して結果を再注入
/// （web の `WebCompletionSink` 相当）はしない:
/// - REST には per-session の直列化が無い（web は `run_and_deliver_serialized`、Discord は
///   `SessionLocks::spawn_serialized` を持つ）。resume を走らせると同一セッションへの
///   並行 POST と競合し、同じ会話から二重に応答する（RFC §6 の不変条件違反）。
/// - 完了本文は次の POST の `build_conversation_string` で自然に文脈へ載る
///   （heartbeat の「次 tick 拾い」と同じ方式）。
///
/// sink の役目は `sessions.status` の整合だけ: 走行中 subtask がある間はハンドラが
/// `completed` にしないため、最後の subtask が決着した時点でここが完了させる
/// （そうしないとセッションが永久に `active` のまま残る）。
pub struct RestCompletionSink {
    pub db: opencrab_db::Db,
    /// この REST セッションの共有 registry（他に走行中 subtask が無いかの判定用）。
    pub registry: SubtaskRegistry,
}

impl SubtaskCompletionSink for RestCompletionSink {
    fn on_subtask_settled(&self, ev: SubtaskSettled) {
        // 決着（Completed）以外（進捗通知など）でセッションを完了扱いにしてはならない。
        // 進捗はまだ run が回っている最中に飛ぶので、ここで completed にすると
        // 「応答が返る前に completed」を観測させてしまう。web / Nostr の受け口と同じガード。
        if ev.kind != SettleKind::Completed {
            tracing::debug!(
                session_id = %ev.session_id,
                kind = ?ev.kind,
                "rest sink: not a completion, nothing to reconcile"
            );
            return;
        }
        if !ev.session_id.starts_with(REST_SESSION_PREFIX) {
            tracing::debug!(
                session_id = %ev.session_id,
                "rest sink: parent session is not a REST session, nothing to reconcile"
            );
            return;
        }
        // `settle_completed` は sink 発火より前に当該 subtask を registry から除去する。
        // 空でなければ他の subtask がまだ走行中 = セッションは完了ではない。
        if !self.registry.is_empty() {
            tracing::debug!(
                session_id = %ev.session_id,
                running = self.registry.len(),
                "rest sink: subtask settled but others are still running"
            );
            return;
        }
        mark_session_completed(&self.db, &ev.session_id);
    }

    /// 停止（`cancel_subtask`）でも `sessions.status` の整合を取る。
    ///
    /// cancel された subtask は `settle_completed` を通らない（完了ではない）ため、
    /// **最後の走行中 subtask を停止した場合は誰も `completed` にしない** =
    /// セッションが永久に `active` のまま残る。停止も決着の 1 形態として扱い、
    /// 完了時と同じ「他に走行中が無ければ完了」判定を適用する。resume はしない
    /// （停止したのに返信が飛ぶことはない）。
    fn on_subtask_cancelled(&self, ev: SubtaskSettled) {
        if !ev.session_id.starts_with(REST_SESSION_PREFIX) {
            return;
        }
        // `cancel_subtask` は sink 通知より前に registry から除去している。
        if !self.registry.is_empty() {
            tracing::debug!(
                session_id = %ev.session_id,
                running = self.registry.len(),
                "rest sink: subtask cancelled but others are still running"
            );
            return;
        }
        mark_session_completed(&self.db, &ev.session_id);
    }
}

/// セッションを `completed` にする（best-effort）。
fn mark_session_completed(db: &opencrab_db::Db, session_id: &str) {
    if let Ok(conn) = db.lock() {
        conn.execute(
            "UPDATE sessions SET status = 'completed' WHERE id = ?1",
            [session_id],
        )
        .ok();
    }
}

/// 走行中の dispatch subtask が無いときだけセッションを `completed` にする（#169）。
///
/// 非ブロック dispatch により、HTTP 応答を返した時点でまだ background subtask が
/// 走っていることがある。そこで `completed` にすると「完了したのに走行中」という
/// 不整合になるため、走行中は `active` のまま残し、最後の subtask が決着した時点で
/// `RestCompletionSink` が完了させる。
fn complete_session_if_idle(db: &opencrab_db::Db, session_id: &str, registry: &SubtaskRegistry) {
    if !registry.is_empty() {
        tracing::debug!(
            session_id = %session_id,
            running = registry.len(),
            "rest: subtask still running, keeping session active"
        );
        return;
    }
    mark_session_completed(db, session_id);
}

pub async fn send_agent_message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<SendAgentMessageRequest>,
) -> Response {
    // 存在しないエージェントの弾き出し（#632）は `process::run_agent_response`（サーバ側の
    // 単一チョークポイント）が担う。ここでは run が返す `AgentNotFound` を 404 に写像する
    // （下の match）。入口ごとにチェックをコピーしない。

    // 呼び出し元 ID は入口で 1 回だけ正規化し、以降すべて（認可・セッションキー・
    // speaker_id）で同じ値を使う。`is_owner_id` が trim して比較する一方でセッション
    // キーだけ生値を使うと、`" <id> "` が owner にはなれるのに別セッション・別
    // speaker_id として記録される非対称が生まれる。
    let user_id = req.user_id.trim();
    let session_id = format!("{}{}-{}", REST_SESSION_PREFIX, id, user_id);

    // 1. Determine caller identity from trusted_users table.
    //    引く経路は `rest`（#214）。#214 が残していた互換読み（自経路の行が無ければ
    //    従来の `discord` 経路も見る）は #159 で撤去した。**`platform='rest'` の行を
    //    持たない既存の REST 利用者はここで信頼を失う**（判定は web 側と同じ 1 実装）。
    let caller = {
        let conn = state.db.lock().unwrap();
        crate::caller_identity::resolve_caller_identity(
            &conn,
            opencrab_db::queries::TRUSTED_PLATFORM_REST,
            user_id,
            &id,
        )
    };

    let caller_type = match &caller {
        opencrab_actions::CallerIdentity::CoAgent { .. } => "co_agent",
        opencrab_actions::CallerIdentity::TrustedUser => "trusted_user",
        opencrab_actions::CallerIdentity::Owner => "owner",
        _ => "agent",
    };

    // 2. Ensure session exists (create if not).
    {
        let conn = state.db.lock().unwrap();
        let existing = opencrab_db::queries::get_session(&conn, &session_id)
            .ok()
            .flatten();
        if existing.is_none() {
            let session = opencrab_db::queries::SessionRow {
                id: session_id.clone(),
                mode: "autonomous".to_string(),
                theme: "direct_message".to_string(),
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
                    serde_json::json!({"error": format!("Failed to create session: {}", e)}),
                )
                .into_response();
            }
        }
    }

    // 3. Log user message.
    {
        let log = opencrab_db::queries::SessionLogRow {
            id: None,
            agent_id: id.clone(),
            session_id: session_id.clone(),
            log_type: "speech".to_string(),
            content: req.content.clone(),
            speaker_id: Some(user_id.to_string()),
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        let conn = state.db.lock().unwrap();
        if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &log) {
            return Json(serde_json::json!({"error": format!("Failed to log message: {}", e)}))
                .into_response();
        }
    }

    // 4. Check LLM availability.
    if state.llm_router.get().provider_names().is_empty() {
        return Json(serde_json::json!({
            "session_id": session_id,
            "caller_type": caller_type,
            "responses": [],
            "error": "No LLM providers available",
        }))
        .into_response();
    }

    // 5. dispatch 用の共有 registry を確保する（#169）。
    //    `AppState` が session_id キーで保持しているものを借りるため、リクエストを
    //    跨いでも同一 Arc であり、dispatch した subtask を後続リクエストの
    //    `cancel_subtask` から停止できる（使い捨ての DashMap では常に not found）。
    let subtask_registry = state.subtask_registries.registry_for(&session_id);

    // 6. transport のツール実行の実体を capability で引く（#191 段階2 PR4）。
    //    以前はここで `discord_manager` を名指しし、`get_http_for_agent` から
    //    `DiscordGatewayActions` を組み立てていた（Discord の feature の内と外で
    //    同じ束縛を 2 度書いていた）。組み立ては transport 側へ移り、ここは
    //    「登録簿から引いて、あれば使う」だけになる。**未登録・未稼働はどちらも
    //    `None`**（＝ transport 固有ツール無しで会話する）で、移設前と同じ。
    //
    //    停止（`cancel_subtask`）は #157 S2 で gateway 非依存層だけの実装になったので、
    //    Discord の gateway_actions へ registry を渡す必要はもう無い（`SystemGatewayActions`
    //    が上の共有 registry を直接引く）。
    let gateway_actions: Option<Arc<dyn opencrab_gateway::GatewayActions>> = state
        .gateways
        .get(gateway_kinds::DISCORD)
        .and_then(|gw| gw.gateway_actions_for(&id));

    // 7. Build agent context. 本ターンの caller（上で resolve 済み）で index を絞る。
    // 同じ caller を下の RunRequest にも載せる（index と実行権限を一致させる / #352）。
    let (system_prompt, agent_name) = {
        let conn = state.db.lock().unwrap();
        process::build_agent_context(&conn, &id, &caller)
    };

    // 8. Build conversation string.
    let conversation = {
        let conn = state.db.lock().unwrap();
        let eff = opencrab_db::queries::effective_model_for_agent(&conn, &id, &state.default_model)
            .unwrap_or_else(|_| state.default_model.clone());
        let (prov, mdl) = process::split_llm_model_spec(&eff);
        let budget = process::compute_context_budget(&conn, prov, mdl, state.compaction_ratio);
        let raw = match process::build_conversation_string(&conn, &session_id, &id, budget) {
            Ok(s) => s,
            Err(e) => {
                return Json(
                    serde_json::json!({"error": format!("Failed to build conversation: {}", e)}),
                )
                .into_response();
            }
        };
        process::prepend_runtime_context(&raw, "direct_message")
    };

    // 9. Run agent response.
    //     非ブロック dispatch（#152 S3a / #169）を有効化する。長時間ツールは background
    //     subtask になり、HTTP 応答はメインを塞がずに即座に返る。sink は「保存のみ」
    //     （resume / live push なし。取得はセッションログ経由）。
    let sink: Arc<dyn SubtaskCompletionSink> = Arc::new(RestCompletionSink {
        db: state.db.clone(),
        registry: subtask_registry.clone(),
    });
    let mut run_req = opencrab_actions::RunRequest::new(
        &id,
        &agent_name,
        &session_id,
        &system_prompt,
        &conversation,
        "rest",
        caller,
    )
    .with_dispatch(Some(subtask_registry.clone()), sink);
    if let Some(ga) = gateway_actions {
        run_req = run_req.with_gateway_actions(ga);
    }
    let result = process::run_agent_response(&state, run_req).await;

    // 10. Handle result.
    match result {
        Ok(engine_result) => {
            // Log agent response.
            {
                let conn = state.db.lock().unwrap();
                crate::transcript::record_rest_agent_reply(
                    &conn,
                    &id,
                    &session_id,
                    &engine_result.response,
                    engine_result.iterations,
                    engine_result.tool_calls_made,
                );
            }

            // 走行中 subtask が無ければセッションを完了にする（走行中は active のまま。
            // 最後の subtask 決着時に RestCompletionSink が完了させる）。
            complete_session_if_idle(&state.db, &session_id, &subtask_registry);

            Json(serde_json::json!({
                "session_id": session_id,
                "caller_type": caller_type,
                "responses": [{
                    "agent_id": id,
                    "content": engine_result.response,
                }],
            }))
            .into_response()
        }
        Err(e) => {
            // #632: 存在しないエージェントはチョークポイントで弾かれる。404 に写像する。
            if let Some(nf) = e.downcast_ref::<AgentNotFound>() {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": nf.to_string()})),
                )
                    .into_response();
            }
            tracing::error!(agent_id = %id, error = %e, "Agent response failed");
            // エラー時も同様: 走行中 subtask（エラー前に dispatch 済み）があれば
            // 完了扱いにしない。
            complete_session_if_idle(&state.db, &session_id, &subtask_registry);
            Json(serde_json::json!({
                "session_id": session_id,
                "caller_type": caller_type,
                "responses": [{
                    "agent_id": id,
                    "content": format!("(Error: {})", e),
                }],
            }))
            .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencrab_actions::{SettleKind, SpawnedSubtask, SubtaskLifecycle};

    fn db_with_session(session_id: &str) -> opencrab_db::Db {
        let conn = opencrab_db::init_memory().unwrap();
        conn.execute(
            "INSERT INTO sessions (id, theme, status, created_at, updated_at) \
             VALUES (?1, 't', 'active', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [session_id],
        )
        .unwrap();
        opencrab_db::Db::from_connection(conn)
    }

    fn status_of(db: &opencrab_db::Db, session_id: &str) -> String {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status FROM sessions WHERE id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn settled(session_id: &str, kind: SettleKind) -> SubtaskSettled {
        SubtaskSettled {
            session_id: session_id.to_string(),
            agent_id: "agent-a".to_string(),
            subtask_id: "st-1".to_string(),
            exit_reason: "cancelled".to_string(),
            kind,
            reply_target: None,
            caller: opencrab_actions::CallerIdentity::Agent,
        }
    }

    /// [P1 回帰] 最後の走行中 subtask が **cancel** されたときも session を
    /// `completed` にする（cancel は `settle_completed` を通らないため、これが
    /// 無いと `sessions.status` が永久に `active` のまま残る）。
    #[tokio::test]
    async fn cancel_reconciles_session_status() {
        let session_id = "agent-msg-agent-a-u1";
        let db = db_with_session(session_id);
        let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
        let sink = RestCompletionSink {
            db: db.clone(),
            registry: registry.clone(),
        };

        assert_eq!(status_of(&db, session_id), "active");
        // cancel_subtask は通知より前に registry から除去する（= もう走行中はない）。
        sink.on_subtask_cancelled(settled(session_id, SettleKind::Cancelled));
        assert_eq!(
            status_of(&db, session_id),
            "completed",
            "最後の subtask を停止したのにセッションが active のまま残る"
        );
    }

    /// 進捗通知（Progress）でセッションを完了扱いにしてはならない。
    ///
    /// 進捗はまだ run が回っている最中に飛ぶ。ここで completed にすると、応答が
    /// 返る前に `sessions.status` を見たクライアントが「完了した」と誤認する。
    /// #175 S1 で進捗報告ツールが全経路に露出し、REST の受け口にも Progress が
    /// 届くようになったので、web / Nostr と同じガードが要る。
    #[tokio::test]
    async fn progress_does_not_complete_the_session() {
        let session_id = "agent-msg-agent-a-u1";
        let db = db_with_session(session_id);
        let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
        let sink = RestCompletionSink {
            db: db.clone(),
            registry: registry.clone(),
        };

        assert_eq!(status_of(&db, session_id), "active");
        sink.on_subtask_settled(settled(session_id, SettleKind::Progress));
        assert_eq!(
            status_of(&db, session_id),
            "active",
            "進捗通知でセッションが完了扱いにされている（run はまだ回っている）"
        );

        // 決着（Completed）なら従来どおり完了にする（ガードが効きすぎていないこと）。
        sink.on_subtask_settled(settled(session_id, SettleKind::Completed));
        assert_eq!(status_of(&db, session_id), "completed");
    }

    /// 他に走行中 subtask が残っているあいだは停止でも完了にしない。
    #[tokio::test]
    async fn cancel_keeps_session_active_while_others_run() {
        let session_id = "agent-msg-agent-a-u1";
        let db = db_with_session(session_id);
        let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
        let handle = tokio::spawn(std::future::pending::<()>());
        registry.insert(
            "st-other".to_string(),
            SpawnedSubtask {
                abort_handle: handle.abort_handle(),
                session_id: "subtask-st-other".to_string(),
                parent_session_id: session_id.to_string(),
                agent_id: "agent-a".to_string(),
                label: "other".to_string(),
                tool_name: "spawn_subtask".to_string(),
                started_at: std::time::Instant::now(),
                reply_target: None,
                caller: opencrab_actions::CallerIdentity::Agent,
                lifecycle: SubtaskLifecycle::new(),
            },
        );
        let sink = RestCompletionSink {
            db: db.clone(),
            registry: registry.clone(),
        };

        sink.on_subtask_cancelled(settled(session_id, SettleKind::Cancelled));
        assert_eq!(status_of(&db, session_id), "active");
        handle.abort();
    }

    /// 非 REST セッション（web-* / heartbeat-*）は対象外（誤って触らない）。
    #[tokio::test]
    async fn cancel_ignores_non_rest_sessions() {
        let session_id = "web-agent-a-conv1";
        let db = db_with_session(session_id);
        let registry: SubtaskRegistry = Arc::new(dashmap::DashMap::new());
        let sink = RestCompletionSink {
            db: db.clone(),
            registry,
        };
        sink.on_subtask_cancelled(settled(session_id, SettleKind::Cancelled));
        assert_eq!(status_of(&db, session_id), "active");
    }
}
