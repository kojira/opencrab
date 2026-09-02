//! [`AgentRuntime`] の `AppState` 実装（#156 S1）。
//!
//! 以前は同じ本体が `agent_runner_impl.rs`（discord）/ `nostr_runner_impl.rs` /
//! `web_runner_impl.rs` の 3 箇所にコピーされていた。ゲートウェイ非依存な実行・
//! セッション管理はここ 1 箇所だけが持ち、各ゲートウェイの impl はそれぞれの語彙を
//! 含むメソッドだけを実装する。
//!
//! discord feature の有無に関わらずコンパイルされる（nostr / web も同じ実装を使う）。

use async_trait::async_trait;

use opencrab_actions::AgentRuntime;

use crate::process;
use crate::AppState;

/// 呼び出し側が渡した実 system / runtime で envelope を解く（Owner 固定や汎用 prepend はしない）。
fn resolve_runtime_envelope(
    state: &AppState,
    conn: &rusqlite::Connection,
    agent_id: &str,
    session_id: &str,
    system_prompt: &str,
    runtime_context_text: &str,
) -> Result<process::ContextBudgetEnvelope, opencrab_core::context_budget::ContextBudgetError> {
    let functions_tokens = process::core_functions_tokens()?;
    process::resolve_agent_request_envelope(process::RequestEnvelopeArgs {
        conn,
        agent_id,
        session_id,
        default_model: &state.default_model,
        policy: &state.context_budget_policy(),
        system_prompt,
        runtime_context_text,
        functions_tokens,
        entrypoint: "agent_runtime",
    })
}

#[async_trait]
impl AgentRuntime for AppState {
    async fn run_agent_response(
        &self,
        req: opencrab_actions::RunRequest,
    ) -> anyhow::Result<opencrab_core::EngineResult> {
        process::run_agent_response(self, req).await
    }

    fn build_agent_context(
        &self,
        agent_id: &str,
        caller: &opencrab_actions::CallerIdentity,
    ) -> (String, String) {
        let conn = self.db.lock().unwrap();
        process::build_agent_context(&conn, agent_id, caller)
    }

    fn build_conversation_string(
        &self,
        session_id: &str,
        agent_id: &str,
        _context_budget_tokens: usize,
        system_prompt: &str,
        runtime_context_text: &str,
    ) -> anyhow::Result<String> {
        let conn = self.db.lock().unwrap();
        let env = resolve_runtime_envelope(
            self,
            &conn,
            agent_id,
            session_id,
            system_prompt,
            runtime_context_text,
        )?;
        process::build_conversation_string_with_waters(
            &conn,
            session_id,
            agent_id,
            env.conversation_high,
            env.conversation_low,
            process::include_memory_index(&env),
        )
    }

    fn context_budget_tokens(
        &self,
        agent_id: &str,
        session_id: &str,
        system_prompt: &str,
        runtime_context_text: &str,
    ) -> Result<usize, opencrab_core::context_budget::ContextBudgetError> {
        let conn = self.db.lock().unwrap();
        let env = resolve_runtime_envelope(
            self,
            &conn,
            agent_id,
            session_id,
            system_prompt,
            runtime_context_text,
        )?;
        Ok(env.conversation_high)
    }

    fn has_llm_providers(&self) -> bool {
        !self.llm_router.get().provider_names().is_empty()
    }

    /// `agents` 行の有無を返す（#632）。判定はこの 1 実装に集約する。
    ///
    /// ロック取得は他の読み取りメソッド（`context_budget_tokens` 等）に倣って `unwrap`
    /// する（ロック毒化は panic → 500 が正しい）。**DB クエリのエラーは `Ok(false)` に
    /// 潰さず `?` で伝播させる**: 潰すと一過性の DB エラーで実在するエージェントが 404 に
    /// なってしまう。サーバ側チョークポイント（`process::run_agent_response` の
    /// `get_agent(...)?`）と同じ方針で、404 とは別のエラーとして扱えるようにする。
    fn agent_exists(&self, agent_id: &str) -> anyhow::Result<bool> {
        let conn = self.db.lock().unwrap();
        Ok(opencrab_db::queries::get_agent(&conn, agent_id)?.is_some())
    }

    fn session_locks(&self) -> std::sync::Arc<opencrab_actions::SessionLocks> {
        // #588 Stage 2: プロセス全体で 1 つの共有実体を返す（clone は Arc の参照カウント増加）。
        self.session_locks.clone()
    }

    fn subtask_registry_for(&self, session_id: &str) -> opencrab_actions::SubtaskRegistry {
        self.subtask_registries.registry_for(session_id)
    }

    // ---- 転記（#42: 行の形は transcript モジュールが所有。#158 S3 で gateway 非依存に）

    /// #284 P0-3: ユーザー発言の記録だけは成否を返す（他の転記は best-effort のまま）。
    /// ロック取得に失敗した場合も「記録できていない」ので `false`。
    fn record_inbound_message(
        &self,
        source: opencrab_actions::TranscriptSource,
        record: &opencrab_actions::InboundMessageRecord<'_>,
    ) -> bool {
        match self.db.lock() {
            Ok(conn) => crate::transcript::record_inbound_message(&conn, source, record),
            Err(e) => {
                tracing::error!(
                    session_id = %record.session_id,
                    "failed to lock the database to record an inbound message: {e}"
                );
                false
            }
        }
    }

    /// 共通の受信フック（#156 S4）。購読者は今のところ**ピアレビュー返信の回収** 1 つ。
    ///
    /// 記録（`record_inbound_message`）と分けているのは、こちらが台帳への**副作用**で、
    /// 転記の可否ポリシーとは独立に走る必要があるため。購読者が増えたらこのメソッドの
    /// 中に足す（各ゲートウェイの受信処理は触らない）。
    fn on_inbound_message(
        &self,
        source: opencrab_actions::TranscriptSource,
        agent_id: &str,
        record: &opencrab_actions::InboundMessageRecord<'_>,
    ) {
        crate::peer_review::harvest_inbound_reply(&self.db, source, agent_id, record);
    }

    fn record_outbound_reply(
        &self,
        source: opencrab_actions::TranscriptSource,
        record: &opencrab_actions::OutboundReplyRecord<'_>,
    ) {
        if let Ok(conn) = self.db.lock() {
            crate::transcript::record_outbound_reply(&conn, source, record);
        }
    }

    fn record_interaction_response(
        &self,
        agent_id: &str,
        session_id: &str,
        record: &opencrab_actions::InteractionRecord<'_>,
    ) {
        if let Ok(conn) = self.db.lock() {
            crate::transcript::record_interaction_response(&conn, agent_id, session_id, record);
        }
    }

    fn ensure_session(
        &self,
        session_id: &str,
        agent_ids: &[String],
        theme: &str,
        metadata_json: &str,
        mode: &str,
    ) {
        let Ok(conn) = self.db.lock() else { return };
        if let Ok(Some(existing)) = opencrab_db::queries::get_session(&conn, session_id) {
            // 既存セッションに metadata が無ければ補完する（後方互換）。mode は触らない。
            if existing.metadata_json.is_none() {
                opencrab_db::queries::update_session_metadata(
                    &conn,
                    session_id,
                    metadata_json,
                    theme,
                )
                .ok();
            }
            return;
        }
        let session = opencrab_db::queries::SessionRow {
            id: session_id.to_string(),
            mode: mode.to_string(),
            theme: theme.to_string(),
            phase: "active".to_string(),
            turn_number: 0,
            status: "active".to_string(),
            participant_ids_json: serde_json::to_string(agent_ids).unwrap_or_default(),
            facilitator_id: None,
            done_count: 0,
            max_turns: None,
            metadata_json: Some(metadata_json.to_string()),
        };
        opencrab_db::queries::insert_session(&conn, &session).ok();
    }

    fn session_theme(&self, session_id: &str) -> Option<String> {
        self.db
            .lock()
            .ok()
            .and_then(|conn| {
                opencrab_db::queries::get_session(&conn, session_id)
                    .ok()
                    .flatten()
            })
            .map(|s| s.theme)
    }

    fn mark_interaction_status(
        &self,
        interaction_id: &str,
        status: &str,
        response_json: Option<&str>,
        responder_id: Option<&str>,
    ) {
        if let Ok(conn) = self.db.lock() {
            opencrab_db::queries::update_pending_interaction_status(
                &conn,
                interaction_id,
                status,
                response_json,
                responder_id,
            )
            .ok();
        }
    }

    fn cleanup_stale_interactions(&self) {
        let Ok(conn) = self.db.lock() else { return };
        match opencrab_db::queries::cleanup_stale_pending_interactions(&conn) {
            Ok(closed) => log_closed_interactions(&closed, None),
            Err(e) => tracing::warn!("cleanup_stale_pending_interactions failed: {e}"),
        }
    }

    fn cleanup_stale_interactions_for_agent(&self, agent_id: &str) {
        let Ok(conn) = self.db.lock() else { return };
        match opencrab_db::queries::cleanup_stale_pending_interactions_for_agent(&conn, agent_id) {
            Ok(closed) => log_closed_interactions(&closed, Some(agent_id)),
            Err(e) => {
                tracing::warn!(agent_id = %agent_id, "cleanup_stale_pending_interactions failed: {e}")
            }
        }
    }
}

/// 閉じた保留対話を 1 件ずつ残す（#196）。
///
/// 件数だけだと「どの会話の応答が捨てられたか」が後から追えない。`session_id` は
/// #196 で挿入時に埋めるようにしたので、ここで意味のある値が出る。
fn log_closed_interactions(
    closed: &[opencrab_db::queries::ClosedInteraction],
    scope: Option<&str>,
) {
    if closed.is_empty() {
        return;
    }
    for c in closed {
        tracing::info!(
            interaction_id = %c.id,
            agent_id = %c.agent_id,
            session_id = %c.session_id,
            platform = %c.platform,
            channel_id = %c.channel_id,
            surface_id = %c.surface_id,
            "Closed stale pending interaction as timed out (no in-memory registration to resume)"
        );
    }
    tracing::info!(
        count = closed.len(),
        scope = scope.unwrap_or("all-agents"),
        "Cleaned up stale pending interactions"
    );
}
