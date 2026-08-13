//! クレート内テスト用の [`WebAgentRunner`] 差し替え実装（LLM も DB も使わない）。
//!
//! `respond` と `sink` の両モジュールから使うため、テスト専用の共有モジュールとして
//! 置く（`#[cfg(test)]`）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use opencrab_actions::{AgentRuntime, CallerIdentity, RunRequest};
use opencrab_core::EngineResult;

use crate::gateway::WebGateway;
use crate::runner::WebAgentRunner;

/// `run_agent_response` の観測 1 件。
#[derive(Clone)]
pub struct RunObservation {
    pub session_id: String,
    pub system_prompt: String,
    pub conversation: String,
    /// registry と sink の両方が載っているか（非ブロック dispatch が有効か）。
    pub dispatch_enabled: bool,
    /// run に載った登録簿そのもの。停止（cancel）が届くには、これが
    /// `WebGateway::registry_for(session_id)` と**同一の Arc** でなければならない。
    ///
    /// 中身（`SpawnedSubtask`）は `Debug` を実装しないため、`Debug` は手実装で伏せる。
    pub subtask_registry: Option<opencrab_actions::SubtaskRegistry>,
    /// この run の呼び出し元（#298）。resume が元の権限を落としていないかの検査に使う。
    pub caller: CallerIdentity,
}

impl std::fmt::Debug for RunObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunObservation")
            .field("session_id", &self.session_id)
            .field("system_prompt", &self.system_prompt)
            .field("conversation", &self.conversation)
            .field("dispatch_enabled", &self.dispatch_enabled)
            .field("subtask_registry", &self.subtask_registry.is_some())
            .field("caller", &self.caller)
            .finish()
    }
}

/// 転記された応答 1 件。
#[derive(Clone, Debug)]
pub struct ReplyObservation {
    pub agent_id: String,
    pub session_id: String,
    pub text: String,
}

/// 記録されたユーザ発話 1 件（ハンドラが渡した `user_id` の観測点）。
///
/// `user_id` の正規化がハンドラ経路で効いていることは、正規化関数を単体で叩いても
/// 検出できない（ハンドラが正規化を通さなくなっても単体テストは緑）。認可判定
/// （[`WebAgentRunner::resolve_caller`]）と DB 記録の両方に**同じ正規化済みの値**が
/// 渡ることを見るため、両方の引数を記録する。
#[derive(Clone, Debug)]
pub struct UserMessageObservation {
    pub agent_id: String,
    pub session_id: String,
    pub user_id: String,
    pub content: String,
}

/// 認可判定の呼び出し 1 件（渡された `user_id` の観測点）。
#[derive(Clone, Debug)]
pub struct CallerLookup {
    pub agent_id: String,
    pub user_id: String,
}

#[derive(Clone)]
pub struct FakeRunner {
    gateway: Arc<WebGateway>,
    /// `Ok` なら応答本文、`Err` ならエラーメッセージ。
    response: Result<String, String>,
    /// `resolve_caller` が返す権限（レスポンスの `caller_type` の由来）。
    caller: CallerIdentity,
    /// `has_llm_providers` の返り値（false でプロバイダ未設定の分岐を試す）。
    has_llm_providers: bool,
    /// `Some` なら `ensure_web_session` がこのメッセージで失敗する。
    ensure_session_error: Option<String>,
    /// `Some` なら `record_user_message` がこのメッセージで失敗する。
    record_user_message_error: Option<String>,
    runs: Arc<Mutex<Vec<RunObservation>>>,
    replies: Arc<Mutex<Vec<ReplyObservation>>>,
    user_messages: Arc<Mutex<Vec<UserMessageObservation>>>,
    caller_lookups: Arc<Mutex<Vec<CallerLookup>>>,
    delay: Duration,
    inflight: Arc<AtomicUsize>,
    max_inflight: Arc<AtomicUsize>,
    /// #588 Stage 2: 1 つだけ保持し `session_locks()` は毎回この clone を返す。
    /// trait の契約（プロセス全体で 1 実体を共有）を fake でも守るため（呼ぶたびに
    /// 新実体を返すと「2 回呼んで同じ instance」を期待するテストが静かに直列化を失う）。
    session_locks: Arc<opencrab_actions::SessionLocks>,
}

impl FakeRunner {
    pub fn new(response: &str) -> Self {
        Self::with_result(Ok(response.to_string()))
    }

    pub fn failing(message: &str) -> Self {
        Self::with_result(Err(message.to_string()))
    }

    fn with_result(response: Result<String, String>) -> Self {
        Self {
            gateway: Arc::new(WebGateway::new()),
            response,
            caller: CallerIdentity::Agent,
            has_llm_providers: true,
            ensure_session_error: None,
            record_user_message_error: None,
            runs: Arc::new(Mutex::new(Vec::new())),
            replies: Arc::new(Mutex::new(Vec::new())),
            user_messages: Arc::new(Mutex::new(Vec::new())),
            caller_lookups: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::ZERO,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_inflight: Arc::new(AtomicUsize::new(0)),
            session_locks: Arc::new(opencrab_actions::SessionLocks::new()),
        }
    }

    pub fn with_delay(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }

    /// `resolve_caller` の返り値を差し替える（`caller_type` の由来を固定するため）。
    pub fn with_caller(mut self, caller: CallerIdentity) -> Self {
        self.caller = caller;
        self
    }

    /// セッション用意を失敗させる（ハンドラの早期リターン分岐）。
    pub fn failing_ensure_session(mut self, message: &str) -> Self {
        self.ensure_session_error = Some(message.to_string());
        self
    }

    /// ユーザ発話の記録を失敗させる（ハンドラの早期リターン分岐）。
    pub fn failing_record_user_message(mut self, message: &str) -> Self {
        self.record_user_message_error = Some(message.to_string());
        self
    }

    /// LLM プロバイダ未設定にする（ハンドラが実行せずにエラーを返す分岐）。
    pub fn without_llm_provider(mut self) -> Self {
        self.has_llm_providers = false;
        self
    }

    pub fn runs(&self) -> Vec<RunObservation> {
        self.runs.lock().unwrap().clone()
    }

    pub fn replies(&self) -> Vec<ReplyObservation> {
        self.replies.lock().unwrap().clone()
    }

    pub fn user_messages(&self) -> Vec<UserMessageObservation> {
        self.user_messages.lock().unwrap().clone()
    }

    pub fn caller_lookups(&self) -> Vec<CallerLookup> {
        self.caller_lookups.lock().unwrap().clone()
    }

    pub fn max_inflight(&self) -> usize {
        self.max_inflight.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AgentRuntime for FakeRunner {
    async fn run_agent_response(&self, req: RunRequest) -> Result<EngineResult> {
        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_inflight.fetch_max(now, Ordering::SeqCst);
        self.runs.lock().unwrap().push(RunObservation {
            session_id: req.session_id.clone(),
            system_prompt: req.system_prompt.clone(),
            conversation: req.conversation.clone(),
            dispatch_enabled: req.completion_sink.is_some() && req.subtask_registry.is_some(),
            subtask_registry: req.subtask_registry.clone(),
            caller: req.caller.clone(),
        });
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        match &self.response {
            Ok(text) => Ok(EngineResult {
                response: text.clone(),
                iterations: 1,
                tool_calls_made: 0,
                stopped_by_limit: false,
                xml_fallback_parses: 0,
            }),
            Err(msg) => Err(anyhow!(msg.clone())),
        }
    }

    fn build_agent_context(&self, _agent_id: &str, _caller: &CallerIdentity) -> (String, String) {
        ("base prompt".to_string(), "テストくん".to_string())
    }

    fn build_conversation_string(
        &self,
        _session_id: &str,
        _agent_id: &str,
        _context_budget_tokens: usize,
    ) -> Result<String> {
        Ok("conversation".to_string())
    }

    fn context_budget_tokens(&self, _agent_id: &str) -> usize {
        1000
    }

    fn has_llm_providers(&self) -> bool {
        self.has_llm_providers
    }

    fn session_locks(&self) -> std::sync::Arc<opencrab_actions::SessionLocks> {
        self.session_locks.clone()
    }

    // ---- 以下は web ゲートウェイの経路が使わない（Discord/Nostr 由来の記録/掃除）。
    //      呼ばれたら配線ミスなので黙って no-op にせず落とす。

    fn record_agent_no_reply(&self, _agent_id: &str, _session_id: &str) {
        unimplemented!("web の fake は NO_REPLY 記録を使わない")
    }

    fn record_inbound_message(
        &self,
        _source: opencrab_actions::TranscriptSource,
        _record: &opencrab_actions::InboundMessageRecord<'_>,
    ) -> bool {
        unimplemented!("web は WebAgentRunner::record_user_message を使う")
    }

    fn on_inbound_message(
        &self,
        _source: opencrab_actions::TranscriptSource,
        _agent_id: &str,
        _record: &opencrab_actions::InboundMessageRecord<'_>,
    ) {
        // 受信フック（#156 S4）。web の受信経路はまだ配線していないので no-op。
    }

    fn record_outbound_reply(
        &self,
        _source: opencrab_actions::TranscriptSource,
        _record: &opencrab_actions::OutboundReplyRecord<'_>,
    ) {
        unimplemented!("web は WebAgentRunner::record_agent_reply を使う")
    }

    fn record_interaction_response(
        &self,
        _agent_id: &str,
        _session_id: &str,
        _record: &opencrab_actions::InteractionRecord<'_>,
    ) {
        unimplemented!("web の fake は A2UI interaction を使わない")
    }

    fn ensure_session(&self, _s: &str, _a: &[String], _t: &str, _m: &str, _mode: &str) {
        unimplemented!("web は ensure_web_session を使う")
    }

    fn session_theme(&self, _session_id: &str) -> Option<String> {
        unimplemented!("web の fake は session_theme を使わない")
    }

    fn mark_interaction_status(
        &self,
        _interaction_id: &str,
        _status: &str,
        _response_json: Option<&str>,
        _responder_id: Option<&str>,
    ) {
        unimplemented!("web の fake は A2UI interaction を使わない")
    }

    fn cleanup_stale_interactions(&self) {
        unimplemented!("web の fake は A2UI interaction を使わない")
    }

    fn cleanup_stale_interactions_for_agent(&self, _agent_id: &str) {
        unimplemented!("web の fake は A2UI interaction を使わない")
    }
}

impl WebAgentRunner for FakeRunner {
    fn resolve_caller(&self, agent_id: &str, user_id: &str) -> CallerIdentity {
        self.caller_lookups.lock().unwrap().push(CallerLookup {
            agent_id: agent_id.to_string(),
            user_id: user_id.to_string(),
        });
        self.caller.clone()
    }

    fn ensure_web_session(&self, _session_id: &str, _agent_id: &str) -> Result<()> {
        match &self.ensure_session_error {
            Some(msg) => Err(anyhow!(msg.clone())),
            None => Ok(()),
        }
    }

    fn record_user_message(
        &self,
        agent_id: &str,
        session_id: &str,
        user_id: &str,
        content: &str,
    ) -> Result<()> {
        self.user_messages
            .lock()
            .unwrap()
            .push(UserMessageObservation {
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                user_id: user_id.to_string(),
                content: content.to_string(),
            });
        match &self.record_user_message_error {
            Some(msg) => Err(anyhow!(msg.clone())),
            None => Ok(()),
        }
    }

    fn record_agent_reply(
        &self,
        agent_id: &str,
        session_id: &str,
        text: &str,
        _iterations: usize,
        _tool_calls_made: usize,
    ) {
        self.replies.lock().unwrap().push(ReplyObservation {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            text: text.to_string(),
        });
    }

    fn web_gateway(&self) -> &Arc<WebGateway> {
        &self.gateway
    }
}
