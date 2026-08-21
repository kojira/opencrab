//! Evaluator: 契約（受け入れ条件）に対する成果物の独立評価（LOOPS 原則 I の verify / II / VI）。
//!
//! generator（SkillEngine の run）とは別の新しい context で LLM を1回呼び、
//! タスク台帳の contract に照らして rubric 評価（score 0-1 + ギャップ説明）を返す。
//! 生成した本人の context の続きで自己採点させない、が設計原則。
//! 評価者には要約でなく**生のトレース**（session_logs の tool_call/tool_result/speech）を渡す。
//!
//! **呼び出しタイミング（#291）**: 対話ターンの処理中には呼ばない。以前は
//! `crates/server/src/process.rs` の run 直後に毎ターン走らせ、結果を
//! `session_logs`（log_type=evaluation）へ書いて次ターンの会話へ混ぜていたが、
//! 採点結果と「次ターンでギャップを埋めろ」という指示文が人間の発言と同じ土俵に
//! 並び、直前のユーザー発言より採点の圧が勝つ事故が起きた。評価は非対話時
//! （スリープ中）に行い、結果は本人が見に行く場所（記憶／タスク台帳）へ置く。
//! このモジュールはその移設先から呼ぶために残してある（移設は別 issue）。

use anyhow::{anyhow, Result};
use opencrab_db::queries::SessionLogRow;
use opencrab_llm_types::{ChatRequest, Message};
use serde::Deserialize;

use crate::engine::LlmClient;
use crate::llm_text::{strip_code_fences, truncate_chars};

/// 評価者に渡すトレースの1エントリ描画上限（chars）。
const TRACE_ENTRY_MAX_CHARS: usize = 700;
/// 評価者に渡すトレースの metadata 描画上限（chars）。tool 名/引数が読める程度。
const TRACE_META_MAX_CHARS: usize = 500;
/// 評価者に渡すトレースのエントリ数上限（直近優先）。
const TRACE_MAX_ENTRIES: usize = 80;
/// 評価者に渡す最終応答の描画上限（chars）。
const RESPONSE_MAX_CHARS: usize = 8000;

fn default_relevant() -> bool {
    true
}

/// rubric 評価の結果。
#[derive(Debug, Clone, Deserialize)]
pub struct Evaluation {
    /// 契約の達成度 (0.0-1.0)。
    pub score: f64,
    /// この run がそもそも契約タスクに取り組んだものか。
    /// false = 無関係な run（別の質問への回答等）— 記録しない。
    #[serde(default = "default_relevant")]
    pub relevant: bool,
    /// 未達のギャップ（何が足りないか）。
    #[serde(default)]
    pub gaps: Vec<String>,
    /// 一行の講評。
    #[serde(default)]
    pub summary: String,
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
            .map(|m| format!(" meta={}", truncate_chars(m, TRACE_META_MAX_CHARS)))
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
- the raw execution trace of ONE run (tool calls, tool results, messages)\n\
- the worker's final response for that run\n\
\n\
First decide relevance: if this run was NOT an attempt to work on the stated \
goal/contract (for example, the worker was answering an unrelated question in the \
same session), set \"relevant\" to false and do not judge it against the contract.\n\
\n\
If relevant, score how completely the CONTRACT is satisfied, based on evidence in \
the trace — not on how confident the final response sounds. Claims without \
supporting evidence in the trace do not count.\n\
\n\
Respond with ONLY a JSON object, no prose, no comments, exactly these fields:\n\
{\"relevant\": true, \"score\": 0.85, \"gaps\": [\"list of concrete unmet criteria or unverified claims\"], \"summary\": \"one sentence\"}\n\
\n\
\"score\" is a number from 0.0 to 1.0, where 1.0 means every acceptance criterion \
is verifiably met. \"gaps\" is an empty array when nothing is missing.";

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
    if !eval.score.is_finite() {
        return Err(anyhow!("evaluator returned non-finite score"));
    }
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
        // 1.4 は 1.0 にクランプ、relevant はデフォルト true
        assert_eq!(eval.score, 1.0);
        assert!(eval.relevant);
        assert_eq!(eval.summary, "done");
    }

    #[tokio::test]
    async fn evaluate_parses_irrelevant_verdict() {
        let llm = MockLlm(
            r#"{"relevant": false, "score": 0.0, "gaps": [], "summary": "unrelated run"}"#
                .to_string(),
        );
        let eval = evaluate_against_contract(&llm, "m", "goal", "contract", "resp", "trace")
            .await
            .unwrap();
        assert!(!eval.relevant);
    }

    #[tokio::test]
    async fn evaluate_errors_on_garbage_and_nan() {
        let llm = MockLlm("I think it looks fine!".to_string());
        assert!(evaluate_against_contract(&llm, "m", "g", "c", "r", "t")
            .await
            .is_err());

        let llm = MockLlm(r#"{"score": null}"#.to_string());
        assert!(evaluate_against_contract(&llm, "m", "g", "c", "r", "t")
            .await
            .is_err());
    }

    #[test]
    fn evaluation_parses_and_defaults() {
        let e: Evaluation =
            serde_json::from_str(r#"{"score": 0.4, "gaps": ["tests not run"], "summary": "s"}"#)
                .unwrap();
        assert_eq!(e.score, 0.4);
        assert_eq!(e.gaps.len(), 1);

        // gaps/summary/relevant 省略でもパースできる
        let e: Evaluation = serde_json::from_str(r#"{"score": 1.0}"#).unwrap();
        assert!(e.gaps.is_empty());
        assert!(e.relevant);
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
