//! `context_budget_check` 観測（#826-A）。
//!
//! 入口・費目・水位・before/after・action/reason を typed レコードと同一の
//! tracing フィールドで残す。

use super::envelope::{BudgetExhaustReason, ContextBudgetEnvelope, LineItems, MemoryIndexDecision};

/// 予算検査の結果アクション。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetCheckAction {
    Allow,
    OmitMemoryIndex,
    Exhausted,
}

impl BudgetCheckAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::OmitMemoryIndex => "omit_memory_index",
            Self::Exhausted => "exhausted",
        }
    }
}

/// `context_budget_check` 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBudgetCheck {
    pub entrypoint: String,
    pub items: LineItems,
    pub input_high: usize,
    pub input_low: usize,
    pub conversation_high: usize,
    pub conversation_low: usize,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub action: BudgetCheckAction,
    pub reason: String,
}

impl ContextBudgetCheck {
    pub fn from_envelope(
        entrypoint: impl Into<String>,
        envelope: &ContextBudgetEnvelope,
        before_tokens: usize,
        after_tokens: usize,
        action: BudgetCheckAction,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            entrypoint: entrypoint.into(),
            items: envelope.items,
            input_high: envelope.water.input_high,
            input_low: envelope.water.input_low,
            conversation_high: envelope.conversation_high,
            conversation_low: envelope.conversation_low,
            before_tokens,
            after_tokens,
            action,
            reason: reason.into(),
        }
    }

    pub fn from_memory_index_decision(
        entrypoint: impl Into<String>,
        envelope: &ContextBudgetEnvelope,
        before_tokens: usize,
    ) -> Self {
        let (action, reason) = match envelope.memory_index_decision {
            MemoryIndexDecision::Inject => (
                BudgetCheckAction::Allow,
                format!(
                    "memory_index_injected entries={} tokens={}",
                    envelope.memory_index_entry_count, envelope.injected_memory_index
                ),
            ),
            MemoryIndexDecision::Omit { reason } => (
                BudgetCheckAction::OmitMemoryIndex,
                format!(
                    "memory_index_omitted reason={reason:?} entries={} tokens={}",
                    envelope.memory_index_entry_count, envelope.memory_index_measured
                ),
            ),
        };
        let after = match action {
            BudgetCheckAction::OmitMemoryIndex => 0,
            _ => before_tokens,
        };
        Self::from_envelope(entrypoint, envelope, before_tokens, after, action, reason)
    }
}

/// 必須費目超過の観測行。
pub fn exhausted_check(
    entrypoint: impl Into<String>,
    items: LineItems,
    input_high: usize,
    input_low: usize,
    before_tokens: usize,
    reason: BudgetExhaustReason,
) -> ContextBudgetCheck {
    ContextBudgetCheck {
        entrypoint: entrypoint.into(),
        items,
        input_high,
        input_low,
        conversation_high: 0,
        conversation_low: 0,
        before_tokens,
        after_tokens: before_tokens,
        action: BudgetCheckAction::Exhausted,
        reason: format!("{reason:?}"),
    }
}

/// `target = "context_budget_check"` で構造化ログを出す。
pub fn emit_context_budget_check(check: &ContextBudgetCheck) {
    tracing::info!(
        target: "context_budget_check",
        entrypoint = %check.entrypoint,
        system = check.items.system,
        runtime_context = check.items.runtime_context,
        functions = check.items.functions,
        output_reserve = check.items.output_reserve,
        memory_index = check.items.memory_index,
        conversation = check.items.conversation,
        input_high = check.input_high,
        input_low = check.input_low,
        conversation_high = check.conversation_high,
        conversation_low = check.conversation_low,
        before = check.before_tokens,
        after = check.after_tokens,
        action = check.action.as_str(),
        reason = %check.reason,
        "context_budget_check"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_budget::envelope::{
        apply_line_items, compute_water_levels, ContextBudgetPolicy, MeasuredLineItems,
        MemoryIndexOmitReason,
    };

    #[test]
    fn check_record_has_entrypoint_items_water_before_after_action_reason() {
        let water = compute_water_levels(200_000, 1_000, &ContextBudgetPolicy::default());
        let env = apply_line_items(
            water,
            MeasuredLineItems {
                system: 10,
                runtime_context: 20,
                functions: 30,
                memory_index: 5_000,
                memory_index_entry_count: 8,
                conversation: 100,
            },
            &ContextBudgetPolicy::default(),
        )
        .unwrap();
        assert!(matches!(
            env.memory_index_decision,
            crate::context_budget::envelope::MemoryIndexDecision::Omit {
                reason: MemoryIndexOmitReason::ExceedsDedicatedCap
            }
        ));
        let check = ContextBudgetCheck::from_memory_index_decision("rest", &env, 5_000);
        assert_eq!(check.entrypoint, "rest");
        assert_eq!(check.items.system, 10);
        assert_eq!(check.input_high, water.input_high);
        assert_eq!(check.input_low, water.input_low);
        assert_eq!(check.before_tokens, 5_000);
        assert_eq!(check.action, BudgetCheckAction::OmitMemoryIndex);
        assert!(check.reason.contains("memory_index_omitted"));
        assert_eq!(check.action.as_str(), "omit_memory_index");
        emit_context_budget_check(&check);
    }

    #[test]
    fn exhausted_check_action_name() {
        let check = exhausted_check(
            "startup",
            LineItems::default(),
            80_000,
            40_000,
            90_000,
            BudgetExhaustReason::MandatoryFixedExceedsInputHigh,
        );
        assert_eq!(check.action.as_str(), "exhausted");
        assert_eq!(check.entrypoint, "startup");
        assert_eq!(check.before_tokens, 90_000);
        assert_eq!(check.after_tokens, 90_000);
    }
}
