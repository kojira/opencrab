use super::{
    completion_requests::mark_completion_request_consumed,
    run_helpers::{classify_call_failure, CallFailure},
    SkillEngine,
};
use crate::engine::types::{LlmCallLog, LlmExchangeLog};
use anyhow::Result;
use opencrab_llm_types::{ChatRequest, ChatResponse, LlmExchange, ProviderToolHistory};

impl SkillEngine {
    /// 一回のprovider交換、監査保存、completion消費を一つの責務として実行する。
    pub(super) async fn execute_exchange(
        &self,
        request: ChatRequest,
        model: &str,
        iterations: usize,
        completion_request_id: Option<&str>,
        pending_event_ids: &mut Vec<String>,
        recovered_response: Option<ChatResponse>,
    ) -> Result<(ChatResponse, ChatRequest, bool)> {
        let request_for_log = request.clone();
        let requested_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let call_start = std::time::Instant::now();
        let replayed_effect = recovered_response.is_some();
        let exchange_result = if let Some(response) = recovered_response {
            Ok(LlmExchange {
                response,
                provider_tool_history: Default::default(),
            })
        } else {
            self.llm.chat_with_history(request).await
        };
        let latency_ms = call_start.elapsed().as_millis() as i64;
        tracing::debug!(
            iteration = iterations,
            latency_ms,
            ok = exchange_result.is_ok(),
            stage = "llm_call",
            "turn: LLM リクエスト 完了（出）"
        );

        let response_result = match &exchange_result {
            Ok(exchange) => Ok(exchange.response.clone()),
            Err(error) => Err(anyhow::anyhow!(error.to_string())),
        };
        let call_failure = classify_call_failure(&response_result, model, self.max_output_tokens);
        let call_log = LlmCallLog {
            request: request_for_log.clone(),
            response: exchange_result
                .as_ref()
                .ok()
                .map(|exchange| exchange.response.clone()),
            error_str: call_failure.as_ref().map(|failure| failure.body.clone()),
            error_code: call_failure.as_ref().map(|failure| failure.code.clone()),
            latency_ms,
            requested_at,
            is_bot_iteration: iterations > 1,
        };
        if !replayed_effect {
            if let Some(cb) = &self.log_callback {
                cb(&call_log);
            }
        }
        let provider_tool_history = exchange_result
            .as_ref()
            .map(|exchange| exchange.provider_tool_history.clone())
            .unwrap_or_else(|_| ProviderToolHistory::default());
        let exchange_log = LlmExchangeLog {
            call: call_log,
            provider_tool_history,
        };
        if !replayed_effect {
            if let Some(cb) = &self.durable_exchange_log_callback {
                let durable_events = if call_failure.is_none() {
                    pending_event_ids.as_slice()
                } else {
                    &[]
                };
                let durable_request_id = call_failure
                    .is_none()
                    .then_some(completion_request_id)
                    .flatten();
                cb(&exchange_log, durable_events, durable_request_id)?;
                if durable_request_id.is_some() {
                    pending_event_ids.clear();
                }
            } else {
                if let Some(cb) = &self.exchange_log_callback {
                    cb(&exchange_log);
                }
                if call_failure.is_none() {
                    mark_completion_request_consumed(
                        self,
                        pending_event_ids,
                        completion_request_id,
                    )?;
                }
            }
        }

        let response = exchange_result?.response;
        if let Some(CallFailure { code, body }) = call_failure {
            tracing::error!(
                iteration = iterations,
                error_code = %code,
                model,
                stage = "turn_failed",
                "turn: LLM 応答が使えないためターン失敗（fail loud）"
            );
            anyhow::bail!(body);
        }
        Ok((response, request_for_log, replayed_effect))
    }
}
