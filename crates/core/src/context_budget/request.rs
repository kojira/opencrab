//! 各 request 前の envelope 合成（#826-A）。
//!
//! 入口固有の再計算は置かない。水位・費目・MI 判定はここだけが
//! [`super::apply_line_items`] を呼び、観測は [`ContextBudgetCheck::from_envelope`]。

use crate::tokens::measure_item_tokens;
use opencrab_llm_types::FunctionDefinition;

use super::envelope::{
    apply_line_items, ContextBudgetEnvelope, ContextBudgetPolicy, LineItems, MeasuredLineItems,
    WaterLevels,
};
use super::error::ContextBudgetError;
use super::observe::{emit_context_budget_check, exhausted_check, ContextBudgetCheck};
use super::{resolve_water_levels, split_llm_model_spec};

/// functions 費目: `ChatRequest.functions` と同じ JSON を 1 回測る。
pub fn measure_functions_tokens(defs: &[FunctionDefinition]) -> Result<usize, ContextBudgetError> {
    let json = serde_json::to_string(defs).map_err(|e| ContextBudgetError::LookupFailed {
        spec: "functions".to_string(),
        cause: e.to_string(),
    })?;
    Ok(measure_item_tokens(&json))
}

/// Memory Index セクションの token と行数。セクションが無ければ (0, 0)。
pub fn measure_memory_index(
    conn: &rusqlite::Connection,
    agent_id: &str,
    session_id: &str,
) -> (usize, usize) {
    match crate::memory_index::build_memory_index_section(conn, agent_id, session_id) {
        Ok(Some(section)) => {
            let tokens = measure_item_tokens(&section);
            let entries = section
                .lines()
                .skip(1)
                .filter(|line| !line.trim().is_empty())
                .count();
            (tokens, entries)
        }
        Ok(None) | Err(_) => (0, 0),
    }
}

/// `apply_line_items` を本番経路で走らせ、観測行を `from_envelope` で残す。
pub fn resolve_request_envelope(
    water: WaterLevels,
    measured: MeasuredLineItems,
    policy: &ContextBudgetPolicy,
    entrypoint: &str,
) -> Result<ContextBudgetEnvelope, ContextBudgetError> {
    match apply_line_items(water, measured, policy) {
        Ok(env) => {
            let check = ContextBudgetCheck::from_memory_index_decision(
                entrypoint,
                &env,
                env.memory_index_measured,
            );
            emit_context_budget_check(&check);
            Ok(env)
        }
        Err(err) => {
            if let ContextBudgetError::Exhausted {
                reason,
                input_high,
                input_low,
                system,
                runtime_context,
                functions,
                output_reserve,
                memory_index,
                ..
            } = &err
            {
                let items = LineItems {
                    system: *system,
                    runtime_context: *runtime_context,
                    functions: *functions,
                    output_reserve: *output_reserve,
                    memory_index: *memory_index,
                    conversation: measured.conversation,
                };
                emit_context_budget_check(&exhausted_check(
                    entrypoint,
                    items,
                    *input_high,
                    *input_low,
                    items.total_with_reserve(),
                    *reason,
                ));
            }
            Err(err)
        }
    }
}

/// [`resolve_agent_request_envelope`] の入力。入口固有の再計算は持たない。
pub struct RequestEnvelopeArgs<'a> {
    pub conn: &'a rusqlite::Connection,
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub default_model: &'a str,
    pub policy: &'a ContextBudgetPolicy,
    pub system_prompt: &'a str,
    pub runtime_context_text: &'a str,
    pub functions_tokens: usize,
    pub entrypoint: &'a str,
}

/// モデル解決 → 水位 → 費目計測 → envelope。
///
/// `effective_model_for_agent` の失敗は既定モデルへ落とさず `LookupFailed`。
pub fn resolve_agent_request_envelope(
    args: RequestEnvelopeArgs<'_>,
) -> Result<ContextBudgetEnvelope, ContextBudgetError> {
    let spec = opencrab_db::queries::effective_model_for_agent(
        args.conn,
        args.agent_id,
        args.default_model,
    )
    .map_err(|e| ContextBudgetError::LookupFailed {
        spec: args.default_model.to_string(),
        cause: e.to_string(),
    })?;
    let (provider, model) = split_llm_model_spec(&spec);
    let water = resolve_water_levels(args.conn, provider, model, args.policy)?;
    let (memory_index, memory_index_entry_count) =
        measure_memory_index(args.conn, args.agent_id, args.session_id);
    let measured = MeasuredLineItems {
        system: measure_item_tokens(args.system_prompt),
        runtime_context: measure_item_tokens(args.runtime_context_text),
        functions: args.functions_tokens,
        memory_index,
        memory_index_entry_count,
        conversation: 0,
    };
    resolve_request_envelope(water, measured, args.policy, args.entrypoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_budget::envelope::{
        compute_water_levels, MemoryIndexDecision, MemoryIndexOmitReason,
        DEFAULT_MEMORY_INDEX_TOKEN_CAP,
    };
    use crate::context_budget::error::CONTEXT_BUDGET_EXHAUSTED;

    #[test]
    fn resolve_request_envelope_emits_from_envelope_fields() {
        let water = compute_water_levels(200_000, 1_000, &ContextBudgetPolicy::default()).unwrap();
        let env = resolve_request_envelope(
            water,
            MeasuredLineItems {
                system: 10,
                runtime_context: 20,
                functions: 30,
                memory_index: DEFAULT_MEMORY_INDEX_TOKEN_CAP + 1,
                memory_index_entry_count: 4,
                conversation: 0,
            },
            &ContextBudgetPolicy::default(),
            "rest",
        )
        .unwrap();
        assert_eq!(
            env.memory_index_decision,
            MemoryIndexDecision::Omit {
                reason: MemoryIndexOmitReason::ExceedsDedicatedCap
            }
        );
        assert_eq!(env.items.system, 10);
        assert_eq!(env.conversation_high, water.input_high - env.fixed);
    }

    #[test]
    fn resolve_request_envelope_exhausted_keeps_unique_name() {
        let policy = ContextBudgetPolicy {
            absolute_cap_a: 1_000,
            ..ContextBudgetPolicy::default()
        };
        let water = compute_water_levels(10_000, 400, &policy).unwrap();
        let err = resolve_request_envelope(
            water,
            MeasuredLineItems {
                system: 300,
                runtime_context: 100,
                functions: 200,
                memory_index: 0,
                memory_index_entry_count: 0,
                conversation: 0,
            },
            &policy,
            "scheduler",
        )
        .unwrap_err();
        assert_eq!(err.name(), CONTEXT_BUDGET_EXHAUSTED);
    }

    /// request 前 functions 超過も `apply_line_items` 経由なら、一意名 + 全費目が Display に載る。
    #[test]
    fn resolve_request_envelope_functions_over_cap_keeps_full_line_items() {
        let policy = ContextBudgetPolicy {
            functions_token_cap: 50,
            ..ContextBudgetPolicy::default()
        };
        let water = compute_water_levels(200_000, 1_000, &policy).unwrap();
        let err = resolve_request_envelope(
            water,
            MeasuredLineItems {
                system: 11,
                runtime_context: 22,
                functions: 51,
                memory_index: 7,
                memory_index_entry_count: 1,
                conversation: 0,
            },
            &policy,
            "run_agent_response",
        )
        .unwrap_err();
        assert_eq!(err.name(), CONTEXT_BUDGET_EXHAUSTED);
        let display = err.to_string();
        assert!(display.starts_with(CONTEXT_BUDGET_EXHAUSTED), "{display}");
        assert!(display.contains("input_high="), "{display}");
        assert!(display.contains("system=11"), "{display}");
        assert!(display.contains("runtime_context=22"), "{display}");
        assert!(display.contains("functions=51"), "{display}");
        assert!(display.contains("output_reserve=1000"), "{display}");
        assert!(display.contains("memory_index=7"), "{display}");
        match err {
            ContextBudgetError::Exhausted {
                input_high,
                system,
                runtime_context,
                functions,
                output_reserve,
                memory_index,
                ..
            } => {
                assert_eq!(input_high, water.input_high);
                assert_eq!(system, 11);
                assert_eq!(runtime_context, 22);
                assert_eq!(functions, 51);
                assert_eq!(output_reserve, 1_000);
                assert_eq!(memory_index, 7);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn measure_functions_tokens_counts_serialized_defs() {
        let defs = vec![FunctionDefinition {
            name: "ping".to_string(),
            description: Some("pong".to_string()),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let tokens = measure_functions_tokens(&defs).unwrap();
        assert!(tokens > 0);
    }
}
