use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use opencrab_actions::{gateway_kinds, SubtaskCompletionSink, SubtaskRegistry, SubtaskSettled};

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
/// （web-gateway の完了 sink 相当）はしない:
/// - REST は request/response で live push 先を持たない。完了本文は次の POST の
///   `build_conversation_string` で自然に文脈へ載る（heartbeat の「次 tick 拾い」と同じ方式）。
///
/// （かつてここには「REST には per-session の直列化が無いので resume を走らせると並行 POST
/// と競合する」と書いていた。#640 で REST も `send_agent_message` / `send_message` が共有の
/// `session_locks.run_serialized` を通るようになり、その前提は解消した。resume をしないのは
/// 直列化の欠如が理由ではなく、上記のとおり push 先を持たないためである。）
///
/// sink の役目は `sessions.status` の整合だけ: 走行中 subtask がある間はハンドラが
/// `completed` にしないため、最後の subtask が決着した時点でここが完了させる
/// （そうしないとセッションが永久に `active` のまま残る）。
pub struct RestCompletionSink {
    pub db: opencrab_db::Db,
    /// この REST セッションの共有 registry（他に走行中 subtask が無いかの判定用）。
    pub registry: SubtaskRegistry,
    /// 継続ターンを回す実行環境（#638）。`run_agent_response` と共有ロックを引く。
    pub state: AppState,
    /// 継続ターンの `RunRequest` に載せるエージェント名。
    pub agent_name: String,
}

/// REST の継続ターンを 1 本走らせ、結果をセッションログへ残す（#638）。
///
/// 同期 POST は既に応答を返しているので live push する口は無い。結果は
/// `record_rest_agent_reply` でセッションログへ永続化し、`GET /api/sessions/{id}/logs` と
/// 次の POST の `build_conversation_string` から見えるようにする。
///
/// **共有ロック（`SessionLocks::run_serialized`）を通す**（#640 / PR #658）。同一セッションへの
/// 並行 POST・別の subtask の継続と直列化され、二重応答は起きない。web / Nostr / Discord と
/// 同じ 1 本のロックで、REST 専用の仕組みは足さない。
async fn run_rest_continuation(
    state: AppState,
    registry: SubtaskRegistry,
    agent_name: String,
    ev: SubtaskSettled,
) {
    let session_id = ev.session_id.clone();
    let agent_id = ev.agent_id.clone();

    // 文脈は DB から組み直す（完了本文は `settle_completed` が永続化済み・RFC §1.3）。
    // 予算失敗は既定へ落とさず、一意名を session_logs に残す。
    let built = (|| -> Result<(String, String), (String, String)> {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return Err((
                    "db_lock_poisoned".into(),
                    format!("rest continuation: db lock poisoned: {e}"),
                ));
            }
        };
        let (sp, _name) = process::build_agent_context(&conn, &agent_id, &ev.caller);
        let runtime_text = process::prepend_runtime_context("", "direct_message");
        let functions_tokens = match process::core_functions_tokens() {
            Ok(n) => n,
            Err(e) => {
                return Err((e.name().to_string(), e.to_string()));
            }
        };
        let env = match process::resolve_agent_request_envelope(process::RequestEnvelopeArgs {
            conn: &conn,
            agent_id: &agent_id,
            session_id: &session_id,
            default_model: &state.default_model,
            policy: &state.context_budget_policy(),
            system_prompt: &sp,
            runtime_context_text: &runtime_text,
            functions_tokens,
            entrypoint: "rest_continuation",
        }) {
            Ok(env) => env,
            Err(e) => {
                return Err((e.name().to_string(), e.to_string()));
            }
        };
        let raw = match process::build_conversation_string_with_waters(
            &conn,
            &session_id,
            &agent_id,
            env.conversation_high,
            env.conversation_low,
            process::include_memory_index(&env),
        ) {
            Ok(c) => c,
            Err(e) => {
                let name = if e
                    .to_string()
                    .contains(opencrab_core::context_budget::CONTEXT_BUDGET_EXHAUSTED)
                {
                    opencrab_core::context_budget::CONTEXT_BUDGET_EXHAUSTED.to_string()
                } else {
                    "conversation_build_failed".into()
                };
                return Err((name, e.to_string()));
            }
        };
        Ok((sp, process::prepend_runtime_context(&raw, "direct_message")))
    })();
    let (system_prompt, conversation) = match built {
        Ok(v) => v,
        Err((error_name, detail)) => {
            tracing::error!(
                session_id = %session_id,
                error_name = %error_name,
                error = %detail,
                "rest continuation stopped"
            );
            record_rest_continuation_error(&state.db, &agent_id, &session_id, &error_name, &detail);
            complete_session_if_idle(&state.db, &session_id, &registry);
            return;
        }
    };

    // 継続であることを生成点で伝える（web / Nostr の `resume_prompt_suffix` と同じ型）。
    let conversation = format!(
        "{conversation}\n[subtask_completed: subtask_id={}, exit_reason={}]",
        ev.subtask_id, ev.exit_reason
    );

    let run_req = opencrab_actions::RunRequest::new(
        &agent_id,
        &agent_name,
        &session_id,
        &system_prompt,
        &conversation,
        "rest",
        // 元のターンの呼び出し元を継承する（#298 / #333）。ここを落とすと継続の瞬間に
        // owner/trusted のツールが消える。
        ev.caller.clone(),
    )
    // 継続ターンからも subtask を投げられるようにする（多段のシェル作業が 1 ターンで
    // 終わらないのが #631 の実情）。sink は同じ形で組み直す。
    .with_dispatch(
        Some(registry.clone()),
        Arc::new(RestCompletionSink {
            db: state.db.clone(),
            registry: registry.clone(),
            state: state.clone(),
            agent_name: agent_name.clone(),
        }) as Arc<dyn SubtaskCompletionSink>,
    );

    let result = state
        .session_locks
        .run_serialized(&session_id, process::run_agent_response(&state, run_req))
        .await;

    match result {
        Ok(engine_result) => {
            // #899: 保存前に NO_REPLY 終端解釈。沈黙は speech を残さない。
            if let Some(body) = opencrab_actions::visible_speech_after_markers(
                &engine_result.response,
                opencrab_actions::DeliveryContext {
                    session_id: &session_id,
                    agent_id: &agent_id,
                    origin: "rest",
                },
            ) {
                if let Ok(conn) = state.db.lock() {
                    crate::transcript::record_rest_agent_reply(
                        &conn,
                        &agent_id,
                        &session_id,
                        &body,
                        engine_result.iterations,
                        engine_result.tool_calls_made,
                    );
                }
            }
        }
        Err(e) => {
            // 黙って落とさない（no-silent-fallback）。継続が失敗したことは記録に残す。
            tracing::warn!(
                session_id = %session_id,
                agent_id = %agent_id,
                error = %e,
                "rest continuation: run failed"
            );
        }
    }

    // 走行中 subtask が無くなったらセッションを完了にする（継続ターンが新たな subtask を
    // 投げていれば、その決着時にまたここへ来る）。
    complete_session_if_idle(&state.db, &session_id, &registry);
}

impl SubtaskCompletionSink for RestCompletionSink {
    fn session_prefix(&self) -> &'static str {
        REST_SESSION_PREFIX
    }
    /// 進捗では継続しない（まだ走っている run の途中で二重に応答してしまう）。転送するのは
    /// Discord だけ（#638）。
    fn forwards_progress(&self) -> bool {
        false
    }
    fn deliver_continuation(&self, ev: SubtaskSettled) {
        // kind の検査も親セッションの検査も `dispatch_settled`（#638）が済ませている。
        //
        // **「他に走行中の subtask があるか」は継続の条件にしない**（#638 の裁定）。以前ここに
        // あった `registry.is_empty()` ゲートは「継続しない」設計に付随した status 整合用で、
        // 継続を入れるなら「最後の 1 本が終わるまで何も返さない」ことになる。web / Nostr と
        // 同じく完了 1 本ごとに継続する（ドリブルは実測で再現していない）。status の整合は
        // 継続ターンが終わってから、走行中がゼロのときだけ行う。
        let state = self.state.clone();
        let registry = self.registry.clone();
        let agent_name = self.agent_name.clone();
        // sink は同期関数。継続は非同期なので spawn する（web / Nostr と同じ。ここで待つと
        // dispatch した subtask の完了処理を塞ぐ）。
        tokio::spawn(async move {
            run_rest_continuation(state, registry, agent_name, ev).await;
        });
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
fn record_rest_continuation_error(
    db: &opencrab_db::Db,
    agent_id: &str,
    session_id: &str,
    error_name: &str,
    detail: &str,
) {
    let Ok(conn) = db.lock() else {
        tracing::error!(
            session_id = %session_id,
            error_name,
            "rest continuation: cannot persist named error (db lock poisoned)"
        );
        return;
    };
    let row = opencrab_db::queries::SessionLogRow {
        id: None,
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        log_type: "system".to_string(),
        content: format!("{error_name}: {detail}"),
        speaker_id: Some(agent_id.to_string()),
        turn_number: None,
        metadata_json: Some(
            serde_json::json!({ "error_name": error_name, "entrypoint": "rest_continuation" })
                .to_string(),
        ),
        created_at: None,
    };
    if let Err(e) = opencrab_db::queries::insert_session_log(&conn, &row) {
        tracing::error!(
            session_id = %session_id,
            error_name,
            error = %e,
            "rest continuation: failed to persist named error"
        );
    }
}

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
    //    持たない既存の REST 利用者はここで信頼を失う**。
    //
    //    #848: REST のボディ `user_id` は**自称値**（認証済みチャネルが刻む識別子ではない）。
    //    自称値を owner 識別子と平文照合して owner へ昇格させると、owner 識別子を知る到達者が
    //    ボディに書くだけで owner 専用アクション（`execute_shell` 等）へ届く。owner 判定は
    //    「認証済み識別子」経由のみに限定する（案A）ため、REST 専用の
    //    `resolve_rest_caller_identity` を通す（owner 等価へは昇格させない）。gateway 車線
    //    （Nostr / Discord）は認証済み識別子を刻む正しい形なので従来どおり。
    let caller = {
        let conn = state.db.lock().unwrap();
        crate::caller_identity::resolve_rest_caller_identity(&conn, user_id, &id)
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
        let runtime_text = process::prepend_runtime_context("", "direct_message");
        let functions_tokens = match process::core_functions_tokens() {
            Ok(n) => n,
            Err(e) => {
                return Json(serde_json::json!({"error": e.to_string()})).into_response();
            }
        };
        let env = match process::resolve_agent_request_envelope(process::RequestEnvelopeArgs {
            conn: &conn,
            agent_id: &id,
            session_id: &session_id,
            default_model: &state.default_model,
            policy: &state.context_budget_policy(),
            system_prompt: &system_prompt,
            runtime_context_text: &runtime_text,
            functions_tokens,
            entrypoint: "rest",
        }) {
            Ok(env) => env,
            Err(e) => {
                return Json(serde_json::json!({"error": e.to_string()})).into_response();
            }
        };
        let raw = match process::build_conversation_string_with_waters(
            &conn,
            &session_id,
            &id,
            env.conversation_high,
            env.conversation_low,
            process::include_memory_index(&env),
        ) {
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
        // 継続ターン（#638）を回すための実行環境（内部が Arc なので clone は安い）。
        state: state.clone(),
        agent_name: agent_name.clone(),
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
    // 同一セッションへの並行 POST を直列化する（#640）。web / Nostr / Discord / scheduler は
    // 既にこの共有ロック（`state.session_locks` は `AppState` 全体で 1 つ）を通っており、REST
    // だけが `run_agent_response` を直呼びしていた。これは判断ではなく配線漏れで、`State(state)`
    // を受けているのに `session_locks` を一度も参照していなかった。
    //
    // 粒度は **session_id 単位であって global ではない**。同一 session_id への run だけが直列化
    // され、別セッション・別エージェント・別会話は従来どおり並行に走る。global にすると無関係な
    // セッションを巻き込み、「ユーザーの投稿への反応を待たせない」土台を壊す。粒度を広げないこと。
    let result = state
        .session_locks
        .run_serialized(&session_id, process::run_agent_response(&state, run_req))
        .await;

    // 10. Handle result.
    match result {
        Ok(engine_result) => {
            // #899: 保存/返却前に NO_REPLY 終端解釈（配送層 3 箇所と同じ単一実装）を通す。
            // 沈黙（前段が空）は speech を残さず responses も返さない。前段が本文なら本文のみ。
            let speech = opencrab_actions::visible_speech_after_markers(
                &engine_result.response,
                opencrab_actions::DeliveryContext {
                    session_id: &session_id,
                    agent_id: &id,
                    origin: "rest",
                },
            );

            // Log agent response（沈黙でなければ本文のみ）。
            if let Some(body) = &speech {
                let conn = state.db.lock().unwrap();
                crate::transcript::record_rest_agent_reply(
                    &conn,
                    &id,
                    &session_id,
                    body,
                    engine_result.iterations,
                    engine_result.tool_calls_made,
                );
            }

            // 走行中 subtask が無ければセッションを完了にする（走行中は active のまま。
            // 最後の subtask 決着時に RestCompletionSink が完了させる）。
            complete_session_if_idle(&state.db, &session_id, &subtask_registry);

            let responses = match &speech {
                Some(body) => serde_json::json!([{ "agent_id": id, "content": body }]),
                None => serde_json::json!([]),
            };
            Json(serde_json::json!({
                "session_id": session_id,
                "caller_type": caller_type,
                "responses": responses,
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
        let mut state = crate::test_app_state();
        state.db = db.clone();
        let sink = RestCompletionSink {
            db: db.clone(),
            registry: registry.clone(),
            state,
            agent_name: "TestAgent".to_string(),
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
        let mut state = crate::test_app_state();
        state.db = db.clone();
        let sink = RestCompletionSink {
            db: db.clone(),
            registry: registry.clone(),
            state,
            agent_name: "TestAgent".to_string(),
        };

        assert_eq!(status_of(&db, session_id), "active");
        opencrab_actions::dispatch_settled(&sink, settled(session_id, SettleKind::Progress));
        assert_eq!(
            status_of(&db, session_id),
            "active",
            "進捗通知でセッションが完了扱いにされている（run はまだ回っている）"
        );

        // 決着（Completed）は**継続ターン**を起こす（#638）。status の整合は継続ターンが
        // 終わってから（走行中がゼロのとき）行われるので、**同期には完了しない**——これが
        // #638 での挙動変更点。ここでは LLM プロバイダが無いので継続は即座に失敗し、その後
        // `complete_session_if_idle` が走る。spawn された継続を待つため短く poll する。
        opencrab_actions::dispatch_settled(&sink, settled(session_id, SettleKind::Completed));
        let mut settled_status = String::new();
        for _ in 0..40 {
            settled_status = status_of(&db, session_id);
            if settled_status == "completed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(
            settled_status, "completed",
            "継続ターンの後にセッションが完了へ整合されていない"
        );
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
                steerable: false,
            },
        );
        let mut state = crate::test_app_state();
        state.db = db.clone();
        let sink = RestCompletionSink {
            db: db.clone(),
            registry: registry.clone(),
            state,
            agent_name: "TestAgent".to_string(),
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
        let mut state = crate::test_app_state();
        state.db = db.clone();
        let sink = RestCompletionSink {
            db: db.clone(),
            registry,
            state,
            agent_name: "TestAgent".to_string(),
        };
        sink.on_subtask_cancelled(settled(session_id, SettleKind::Cancelled));
        assert_eq!(status_of(&db, session_id), "active");
    }
}
