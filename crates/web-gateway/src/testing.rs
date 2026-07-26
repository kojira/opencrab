//! クレート内テスト用の [`WebAgentRunner`] 差し替え実装（LLM も DB も使わない）。
//!
//! `respond` と `sink` の両モジュールから使うため、テスト専用の共有モジュールとして
//! 置く（`#[cfg(test)]`）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use opencrab_actions::{CallerIdentity, RunRequest};
use opencrab_core::EngineResult;

use crate::gateway::WebGateway;
use crate::runner::WebAgentRunner;

/// `run_agent_response` の観測 1 件。
#[derive(Clone, Debug)]
pub struct RunObservation {
    pub session_id: String,
    pub system_prompt: String,
    pub conversation: String,
    /// registry と sink の両方が載っているか（非ブロック dispatch が有効か）。
    pub dispatch_enabled: bool,
}

/// 転記された応答 1 件。
#[derive(Clone, Debug)]
pub struct ReplyObservation {
    pub agent_id: String,
    pub session_id: String,
    pub text: String,
}

#[derive(Clone)]
pub struct FakeRunner {
    gateway: Arc<WebGateway>,
    /// `Ok` なら応答本文、`Err` ならエラーメッセージ。
    response: Result<String, String>,
    runs: Arc<Mutex<Vec<RunObservation>>>,
    replies: Arc<Mutex<Vec<ReplyObservation>>>,
    delay: Duration,
    inflight: Arc<AtomicUsize>,
    max_inflight: Arc<AtomicUsize>,
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
            runs: Arc::new(Mutex::new(Vec::new())),
            replies: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::ZERO,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_inflight: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_delay(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }

    pub fn runs(&self) -> Vec<RunObservation> {
        self.runs.lock().unwrap().clone()
    }

    pub fn replies(&self) -> Vec<ReplyObservation> {
        self.replies.lock().unwrap().clone()
    }

    pub fn max_inflight(&self) -> usize {
        self.max_inflight.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl WebAgentRunner for FakeRunner {
    async fn run_agent_response(&self, req: RunRequest) -> Result<EngineResult> {
        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_inflight.fetch_max(now, Ordering::SeqCst);
        self.runs.lock().unwrap().push(RunObservation {
            session_id: req.session_id.clone(),
            system_prompt: req.system_prompt.clone(),
            conversation: req.conversation.clone(),
            dispatch_enabled: req.completion_sink.is_some() && req.subtask_registry.is_some(),
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

    fn build_agent_context(&self, _agent_id: &str) -> (String, String) {
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

    fn has_llm_provider(&self) -> bool {
        true
    }

    fn resolve_caller(&self, _agent_id: &str, _user_id: &str) -> CallerIdentity {
        CallerIdentity::Agent
    }

    fn ensure_session(&self, _session_id: &str, _agent_id: &str) -> Result<()> {
        Ok(())
    }

    fn record_user_message(
        &self,
        _agent_id: &str,
        _session_id: &str,
        _user_id: &str,
        _content: &str,
    ) -> Result<()> {
        Ok(())
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
