//! [`AgentRuntime`] の `AppState` 実装（#156 S1）。
//!
//! 以前は同じ本体が `agent_runner_impl.rs`（discord）/ `nostr_runner_impl.rs` /
//! `web_runner_impl.rs` の 3 箇所にコピーされていた。ゲートウェイ非依存な実行・
//! セッション管理はここ 1 箇所だけが持ち、各ゲートウェイの impl はそれぞれの語彙を
//! 含むメソッドだけを実装する。
//!
//! discord feature の有無に関わらずコンパイルされる（nostr / web も同じ実装を使う）。

use async_trait::async_trait;

use opencrab_actions::{
    AgentRuntime, ModelAdminError, ModelAdministration, ModelSnapshot, SessionModelList,
    TurnModelProfile,
};

use crate::process;
use crate::AppState;

/// 呼び出し側が渡した実 system / runtime で envelope を解く（Owner 固定や汎用 prepend はしない）。
fn resolve_runtime_envelope(
    state: &AppState,
    conn: &rusqlite::Connection,
    agent_id: &str,
    session_id: &str,
    model: &str,
    system_prompt: &str,
    runtime_context_text: &str,
) -> Result<process::ContextBudgetEnvelope, opencrab_core::context_budget::ContextBudgetError> {
    let functions_tokens = process::core_functions_tokens()?;
    process::resolve_agent_request_envelope(process::RequestEnvelopeArgs {
        conn,
        agent_id,
        session_id,
        default_model: &state.default_model,
        pinned_model: Some(model),
        policy: &state.context_budget_policy(),
        system_prompt,
        runtime_context_text,
        functions_tokens,
        entrypoint: "agent_runtime",
    })
}

#[async_trait]
impl ModelAdministration for AppState {
    async fn list_models(&self, agent_id: &str) -> Result<ModelSnapshot, ModelAdminError> {
        let models = crate::api::llm::available_models(self)
            .await
            .map_err(|_| ModelAdminError::Internal)?;
        let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
        let agent = opencrab_db::queries::get_agent(&conn, agent_id)
            .map_err(|_| ModelAdminError::Internal)?
            .ok_or(ModelAdminError::Internal)?;
        let configured_model = agent.model;
        let current_model = configured_model
            .clone()
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| self.default_model.clone());
        Ok(ModelSnapshot {
            models,
            configured_model,
            current_model,
            default_model: self.default_model.clone(),
        })
    }

    async fn set_model(
        &self,
        agent_id: &str,
        model: &str,
    ) -> Result<ModelSnapshot, ModelAdminError> {
        let configured_model = {
            let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
            opencrab_db::queries::get_agent(&conn, agent_id)
                .map_err(|_| ModelAdminError::Internal)?
                .ok_or(ModelAdminError::Internal)?
                .model
        };
        let exact_canonical_request = model.trim() == model
            && model
                .split_once(':')
                .is_some_and(|(provider, model_id)| !provider.is_empty() && !model_id.is_empty());
        if exact_canonical_request && configured_model.as_deref() == Some(model) {
            return Ok(ModelSnapshot {
                models: Vec::new(),
                configured_model,
                current_model: model.to_string(),
                default_model: self.default_model.clone(),
            });
        }

        let models = crate::api::llm::available_models(self)
            .await
            .map_err(|_| ModelAdminError::Internal)?;
        let canonical =
            crate::api::llm::resolve_exact_model(&models, model).map_err(|error| match error {
                crate::api::llm::ModelResolutionError::InvalidArgs => ModelAdminError::InvalidArgs,
                crate::api::llm::ModelResolutionError::NotFound => ModelAdminError::NotFound,
                crate::api::llm::ModelResolutionError::Ambiguous => ModelAdminError::Ambiguous,
            })?;

        let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
        let existing = opencrab_db::queries::get_agent(&conn, agent_id)
            .map_err(|_| ModelAdminError::Internal)?
            .ok_or(ModelAdminError::Internal)?;
        if existing.model.as_deref() != Some(canonical.as_str()) {
            let sends_max = self.llm_router.get().sends_max_output_tokens(&canonical);
            crate::process::check_agent_model_change(
                &conn,
                Some(&existing),
                Some(&canonical),
                sends_max,
            )
            .map_err(ModelAdminError::Validation)?;
            let patch = opencrab_db::queries::AgentPatch {
                model: Some(Some(canonical.clone())),
                ..Default::default()
            };
            if !opencrab_db::queries::apply_agent_patch(&conn, agent_id, &patch)
                .map_err(|_| ModelAdminError::Internal)?
            {
                return Err(ModelAdminError::Internal);
            }
        }

        Ok(ModelSnapshot {
            models,
            configured_model: Some(canonical.clone()),
            current_model: canonical,
            default_model: self.default_model.clone(),
        })
    }

    async fn reset_model(&self, agent_id: &str) -> Result<ModelSnapshot, ModelAdminError> {
        let already_reset = {
            let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
            opencrab_db::queries::get_agent(&conn, agent_id)
                .map_err(|_| ModelAdminError::Internal)?
                .ok_or(ModelAdminError::Internal)?
                .model
                .is_none()
        };
        if already_reset {
            return Ok(ModelSnapshot {
                models: Vec::new(),
                configured_model: None,
                current_model: self.default_model.clone(),
                default_model: self.default_model.clone(),
            });
        }

        let models = crate::api::llm::available_models(self)
            .await
            .map_err(|_| ModelAdminError::Internal)?;
        let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
        let existing = opencrab_db::queries::get_agent(&conn, agent_id)
            .map_err(|_| ModelAdminError::Internal)?
            .ok_or(ModelAdminError::Internal)?;
        if existing.model.is_some() {
            crate::process::check_agent_model_change(&conn, Some(&existing), None, true)
                .map_err(ModelAdminError::Validation)?;
            let patch = opencrab_db::queries::AgentPatch {
                model: Some(None),
                ..Default::default()
            };
            if !opencrab_db::queries::apply_agent_patch(&conn, agent_id, &patch)
                .map_err(|_| ModelAdminError::Internal)?
            {
                return Err(ModelAdminError::Internal);
            }
        }
        Ok(ModelSnapshot {
            models,
            configured_model: None,
            current_model: self.default_model.clone(),
            default_model: self.default_model.clone(),
        })
    }

    async fn list_session_models(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<SessionModelList, ModelAdminError> {
        let models = crate::api::llm::available_models(self)
            .await
            .map_err(|_| ModelAdminError::Internal)?;
        let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
        let configured_model = session_model_target(&conn, agent_id, session_id)?;
        let agent_model = self.agent_model(&conn, agent_id)?;
        let snapshot = session_snapshot(models, configured_model, agent_model);
        let realtime_configured_model =
            opencrab_db::queries::get_session_realtime_model(&conn, session_id)
                .map_err(|_| ModelAdminError::Internal)?;
        Ok(SessionModelList {
            realtime_current_model: realtime_configured_model
                .clone()
                .unwrap_or_else(|| snapshot.current_model.clone()),
            realtime_configured_model,
            snapshot,
        })
    }

    async fn set_session_model(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
        model: &str,
    ) -> Result<ModelSnapshot, ModelAdminError> {
        let exact_canonical_request = model.trim() == model
            && model
                .split_once(':')
                .is_some_and(|(provider, model_id)| !provider.is_empty() && !model_id.is_empty());
        {
            let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
            let configured_model = session_model_target(&conn, agent_id, session_id)?;
            if profile == TurnModelProfile::Realtime {
                let realtime_model =
                    opencrab_db::queries::get_session_realtime_model(&conn, session_id)
                        .map_err(|_| ModelAdminError::Internal)?;
                if exact_canonical_request && realtime_model.as_deref() == Some(model) {
                    return self.realtime_snapshot(&conn, agent_id, session_id, Vec::new());
                }
            } else if exact_canonical_request && configured_model.as_deref() == Some(model) {
                let agent_model = self.agent_model(&conn, agent_id)?;
                return Ok(session_snapshot(Vec::new(), configured_model, agent_model));
            }
        }

        let models = crate::api::llm::available_models(self)
            .await
            .map_err(|_| ModelAdminError::Internal)?;
        let canonical =
            crate::api::llm::resolve_exact_model(&models, model).map_err(|error| match error {
                crate::api::llm::ModelResolutionError::InvalidArgs => ModelAdminError::InvalidArgs,
                crate::api::llm::ModelResolutionError::NotFound => ModelAdminError::NotFound,
                crate::api::llm::ModelResolutionError::Ambiguous => ModelAdminError::Ambiguous,
            })?;

        // Membership re-check, validation and write share this one DB lock. The session lock is
        // never taken: a running turn keeps its pinned model; the change applies next turn.
        let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
        let configured_model = session_model_target(&conn, agent_id, session_id)?;
        if profile == TurnModelProfile::Realtime {
            let realtime_model =
                opencrab_db::queries::get_session_realtime_model(&conn, session_id)
                    .map_err(|_| ModelAdminError::Internal)?;
            if realtime_model.as_deref() != Some(canonical.as_str()) {
                crate::process::check_agent_model_change(&conn, None, Some(&canonical), true)
                    .map_err(ModelAdminError::Validation)?;
                self.ensure_realtime_input_high(&conn, agent_id, session_id, &canonical)?;
                if !opencrab_db::queries::set_session_realtime_model(
                    &conn,
                    session_id,
                    Some(&canonical),
                )
                .map_err(|_| ModelAdminError::Internal)?
                {
                    return Err(ModelAdminError::TargetUnavailable);
                }
            }
            return self.realtime_snapshot(&conn, agent_id, session_id, models);
        }
        if configured_model.as_deref() != Some(canonical.as_str()) {
            crate::process::check_agent_model_change(&conn, None, Some(&canonical), true)
                .map_err(ModelAdminError::Validation)?;
            if !opencrab_db::queries::set_session_model(&conn, session_id, Some(&canonical))
                .map_err(|_| ModelAdminError::Internal)?
            {
                return Err(ModelAdminError::TargetUnavailable);
            }
        }
        let agent_model = self.agent_model(&conn, agent_id)?;
        Ok(session_snapshot(models, Some(canonical), agent_model))
    }

    async fn reset_session_model(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
    ) -> Result<ModelSnapshot, ModelAdminError> {
        let conn = self.db.lock().map_err(|_| ModelAdminError::Internal)?;
        let configured_model = session_model_target(&conn, agent_id, session_id)?;
        if profile == TurnModelProfile::Realtime {
            // Reset makes realtime turns follow the session model, which default turns already use.
            if opencrab_db::queries::get_session_realtime_model(&conn, session_id)
                .map_err(|_| ModelAdminError::Internal)?
                .is_some()
                && !opencrab_db::queries::set_session_realtime_model(&conn, session_id, None)
                    .map_err(|_| ModelAdminError::Internal)?
            {
                return Err(ModelAdminError::TargetUnavailable);
            }
            return self.realtime_snapshot(&conn, agent_id, session_id, Vec::new());
        }
        let agent_model = self.agent_model(&conn, agent_id)?;
        if configured_model.is_some() {
            // The inherited model must be usable before the override is dropped.
            opencrab_core::context_budget::ensure_model_context_window_registered(
                &conn,
                &agent_model,
            )
            .and_then(|()| {
                opencrab_core::context_budget::ensure_model_max_output_tokens_registered(
                    &conn,
                    &agent_model,
                )
            })
            .map_err(ModelAdminError::Validation)?;
            if !opencrab_db::queries::set_session_model(&conn, session_id, None)
                .map_err(|_| ModelAdminError::Internal)?
            {
                return Err(ModelAdminError::TargetUnavailable);
            }
        }
        Ok(session_snapshot(Vec::new(), None, agent_model))
    }
}

impl AppState {
    /// The agent's effective model (agent > server default); what a reset session inherits.
    fn agent_model(
        &self,
        conn: &rusqlite::Connection,
        agent_id: &str,
    ) -> Result<String, ModelAdminError> {
        opencrab_db::queries::effective_model_for_agent(conn, agent_id, &self.default_model)
            .map_err(|_| ModelAdminError::Internal)
    }

    /// Realtime-profile result: configured = realtime slot, current = effective realtime model,
    /// default = the session's default-profile model (what an unset slot follows).
    fn realtime_snapshot(
        &self,
        conn: &rusqlite::Connection,
        agent_id: &str,
        session_id: &str,
        models: Vec<String>,
    ) -> Result<ModelSnapshot, ModelAdminError> {
        let configured_model = opencrab_db::queries::get_session_realtime_model(conn, session_id)
            .map_err(|_| ModelAdminError::Internal)?;
        let default_model = opencrab_db::queries::effective_model_for_session(
            conn,
            agent_id,
            session_id,
            &self.default_model,
        )
        .map_err(|_| ModelAdminError::Internal)?;
        Ok(session_snapshot(models, configured_model, default_model))
    }

    /// Rejects a realtime model whose `input_high` under the live context-budget policy is lower
    /// than that of the session's default-profile model, so realtime turns never get a smaller
    /// conversation budget than the session already uses. Equal is accepted.
    fn ensure_realtime_input_high(
        &self,
        conn: &rusqlite::Connection,
        agent_id: &str,
        session_id: &str,
        realtime_model: &str,
    ) -> Result<(), ModelAdminError> {
        use opencrab_core::context_budget::{resolve_water_levels, split_llm_model_spec};
        let policy = self.context_budget_policy();
        let session_model = opencrab_db::queries::effective_model_for_session(
            conn,
            agent_id,
            session_id,
            &self.default_model,
        )
        .map_err(|_| ModelAdminError::Internal)?;
        let input_high = |model: &str| {
            let (provider, model_id) = split_llm_model_spec(model);
            resolve_water_levels(conn, provider, model_id, &policy)
                .map(|levels| levels.input_high)
                .map_err(|error| ModelAdminError::Validation(error.to_string()))
        };
        let realtime_high = input_high(realtime_model)?;
        let session_high = input_high(&session_model)?;
        if realtime_high < session_high {
            return Err(ModelAdminError::Validation(format!(
                "{realtime_model} has a smaller conversation budget ({realtime_high} tokens) than \
                 the session model {session_model} ({session_high} tokens)."
            )));
        }
        Ok(())
    }
}

/// The session's override, provided `session_id` exists and its members are exactly `[agent_id]`.
fn session_model_target(
    conn: &rusqlite::Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<String>, ModelAdminError> {
    if opencrab_db::queries::get_session(conn, session_id)
        .map_err(|_| ModelAdminError::Internal)?
        .is_none()
    {
        return Err(ModelAdminError::TargetUnavailable);
    }
    let members = opencrab_db::queries::list_session_participants(conn, session_id)
        .map_err(|_| ModelAdminError::Internal)?;
    if members.as_slice() != [agent_id] {
        return Err(ModelAdminError::TargetUnavailable);
    }
    opencrab_db::queries::get_session_model(conn, session_id).map_err(|_| ModelAdminError::Internal)
}

fn session_snapshot(
    models: Vec<String>,
    configured_model: Option<String>,
    agent_model: String,
) -> ModelSnapshot {
    ModelSnapshot {
        models,
        current_model: configured_model
            .clone()
            .unwrap_or_else(|| agent_model.clone()),
        configured_model,
        default_model: agent_model,
    }
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
        model: &str,
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
            model,
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
        model: &str,
        system_prompt: &str,
        runtime_context_text: &str,
    ) -> Result<usize, opencrab_core::context_budget::ContextBudgetError> {
        let conn = self.db.lock().unwrap();
        let env = resolve_runtime_envelope(
            self,
            &conn,
            agent_id,
            session_id,
            model,
            system_prompt,
            runtime_context_text,
        )?;
        Ok(env.conversation_high)
    }

    fn session_turn_model(
        &self,
        agent_id: &str,
        session_id: &str,
        profile: TurnModelProfile,
    ) -> anyhow::Result<String> {
        let conn = self.db.lock().unwrap();
        match profile {
            TurnModelProfile::Default => opencrab_db::queries::effective_model_for_session(
                &conn,
                agent_id,
                session_id,
                &self.default_model,
            ),
            TurnModelProfile::Realtime => {
                opencrab_db::queries::effective_realtime_model_for_session(
                    &conn,
                    agent_id,
                    session_id,
                    &self.default_model,
                )
            }
        }
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

    fn has_running_subtask_for_agent(&self, agent_id: &str) -> bool {
        self.subtask_registries.has_running_for_agent(agent_id)
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

    // The generic inbound hook is retained for gateway harnesses, but has no
    // production subscriber after peer-review removal.
    fn on_inbound_message(
        &self,
        _source: opencrab_actions::TranscriptSource,
        _agent_id: &str,
        _record: &opencrab_actions::InboundMessageRecord<'_>,
    ) {
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
#[cfg(test)]
#[path = "agent_runtime_impl/peer_review_removal_test.rs"]
mod peer_review_removal_test;

#[cfg(test)]
#[path = "agent_runtime_impl/session_model_test.rs"]
mod session_model_test;

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
