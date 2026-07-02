//! Evaluator: 契約（受け入れ条件）に対する成果物の独立評価（LOOPS 原則 I の verify / II / VI）。
//!
//! generator（SkillEngine の run）とは別の新しい context で LLM を1回呼び、
//! タスク台帳の contract に照らして rubric 評価（score 0-1 + ギャップ説明）を返す。
//! 生成した本人の context の続きで自己採点させない、が設計原則。
//! 評価者には要約でなく**生のトレース**（session_logs の tool_call/tool_result/speech）を渡す。

use anyhow::{anyhow, Result};
use opencrab_db::queries::SessionLogRow;
use opencrab_llm_types::{ChatRequest, Message};
use serde::Deserialize;

use crate::engine::LlmClient;

/// 評価者に渡すトレースの1エントリ描画上限（chars）。
const TRACE_ENTRY_MAX_CHARS: usize = 700;
/// 評価者に渡すトレースのエントリ数上限（直近優先）。
const TRACE_MAX_ENTRIES: usize = 80;
/// 評価者に渡す最終応答の描画上限（chars）。
const RESPONSE_MAX_CHARS: usize = 8000;

/// rubric 評価の結果。
#[derive(Debug, Clone, Deserialize)]
pub struct Evaluation {
    /// 契約の達成度 (0.0-1.0)。
    pub score: f64,
    /// 未達のギャップ（何が足りないか）。
    #[serde(default)]
    pub gaps: Vec<String>,
    /// 一行の講評。
    #[serde(default)]
    pub summary: String,
}

/// LLM 応答テキストからマークダウンコードフェンスを剥がす。
///
/// `memory_index`/`daily_log_indexer` 等で重複していたイディオムの共通化。
pub fn strip_code_fences(text: &str) -> &str {
    text.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// session_logs の run トレースを評価者向けのプレーンテキストに整形する。
/// エントリ数・文字数で有界。直近 `TRACE_MAX_ENTRIES` 件を時系列順で残す。
pub fn format_trace(logs: &[SessionLogRow]) -> String {
    let start = logs.len().saturating_sub(TRACE_MAX_ENTRIES);
    let mut out = String::new();
    if start > 0 {
        out.push_str(&format!("(… {start} earlier entries omitted …)\n"));
    }
    for log in &logs[start..] {
        let meta = log
            .metadata_json
            .as_deref()
            .map(|m| format!(" meta={}", truncate_chars(m, 300)))
            .unwrap_or_default();
        out.push_str(&format!(
            "[{}]{} {}\n",
            log.log_type,
            meta,
            truncate_chars(&log.content, TRACE_ENTRY_MAX_CHARS),
        ));
    }
    out
}

const EVALUATOR_SYSTEM_PROMPT: &str = "\
You are an independent task evaluator. You did NOT produce the work under review — \
judge it strictly on the evidence, with fresh eyes.\n\
\n\
You will receive:\n\
- the task goal and its contract (acceptance criteria agreed with the user)\n\
- the raw execution trace (tool calls, tool results, messages)\n\
- the worker's final response\n\
\n\
Score how completely the CONTRACT is satisfied, based on evidence in the trace — \
not on how confident the final response sounds. Claims without supporting evidence \
in the trace do not count.\n\
\n\
Respond with ONLY a JSON object, no prose:\n\
{\n\
  \"score\": 0.0-1.0,        // 1.0 = every acceptance criterion verifiably met\n\
  \"gaps\": [\"...\"],         // concrete unmet criteria or unverified claims (empty if none)\n\
  \"summary\": \"...\"          // one sentence\n\
}";

/// 契約に照らして成果物を評価する（新しい context での1ショット呼び出し・ツール無し）。
pub async fn evaluate_against_contract(
    llm: &dyn LlmClient,
    model: &str,
    goal: &str,
    contract: &str,
    final_response: &str,
    trace: &str,
) -> Result<Evaluation> {
    let user_prompt = format!(
        "## Goal\n{goal}\n\n## Contract (acceptance criteria)\n{contract}\n\n\
         ## Execution trace\n{trace}\n\n## Final response\n{}\n\n\
         Evaluate against the contract and return the JSON verdict.",
        truncate_chars(final_response, RESPONSE_MAX_CHARS),
    );

    let request = ChatRequest::new(
        model.to_string(),
        vec![
            Message::system(EVALUATOR_SYSTEM_PROMPT),
            Message::user(&user_prompt),
        ],
    )
    .with_temperature(0.0);

    let response = llm.chat(request).await?;
    let text = response.first_text().unwrap_or_default().to_string();
    let json_str = strip_code_fences(&text);
    let mut eval: Evaluation = serde_json::from_str(json_str)
        .map_err(|e| anyhow!("evaluator returned unparseable verdict: {e}: {json_str}"))?;
    eval.score = eval.score.clamp(0.0, 1.0);
    Ok(eval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use opencrab_llm_types::ChatResponse;

    struct MockLlm(String);

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::text(self.0.clone()))
        }
    }

    #[tokio::test]
    async fn evaluate_parses_fenced_verdict() {
        let llm = MockLlm(
            "```json\n{\"score\": 1.4, \"gaps\": [], \"summary\": \"done\"}\n```".to_string(),
        );
        let eval = evaluate_against_contract(&llm, "m", "goal", "contract", "resp", "trace")
            .await
            .unwrap();
        // 1.4 は 1.0 にクランプされる
        assert_eq!(eval.score, 1.0);
        assert_eq!(eval.summary, "done");
    }

    #[tokio::test]
    async fn evaluate_errors_on_garbage() {
        let llm = MockLlm("I think it looks fine!".to_string());
        let result =
            evaluate_against_contract(&llm, "m", "goal", "contract", "resp", "trace").await;
        assert!(result.is_err());
    }

    #[test]
    fn strip_code_fences_variants() {
        assert_eq!(strip_code_fences("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(strip_code_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fences("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fences("  {\"a\":1}  "), "{\"a\":1}");
    }

    #[test]
    fn evaluation_parses_and_clamps() {
        let e: Evaluation =
            serde_json::from_str(r#"{"score": 0.4, "gaps": ["tests not run"], "summary": "s"}"#)
                .unwrap();
        assert_eq!(e.score, 0.4);
        assert_eq!(e.gaps.len(), 1);

        // gaps/summary 省略でもパースできる
        let e: Evaluation = serde_json::from_str(r#"{"score": 1.0}"#).unwrap();
        assert!(e.gaps.is_empty());
    }

    #[test]
    fn format_trace_bounds_entries() {
        let mk = |i: usize| SessionLogRow {
            id: Some(i as i64),
            agent_id: "a".into(),
            session_id: "s".into(),
            log_type: "speech".into(),
            content: format!("entry {i}"),
            speaker_id: None,
            turn_number: None,
            metadata_json: None,
            created_at: None,
        };
        let logs: Vec<_> = (0..100).map(mk).collect();
        let trace = format_trace(&logs);
        assert!(trace.contains("20 earlier entries omitted"));
        assert!(!trace.contains("entry 19\n"));
        assert!(trace.contains("entry 99"));
    }
}
