//! 文脈予算の fail-loud エラー（#826-A）。
//!
//! 隠れフォールバックは置かない。窓・出力予約の欠落は登録案内、費目超過は
//! 唯一の停止名 [`CONTEXT_BUDGET_EXHAUSTED`] で止める。

use super::envelope::{BudgetExhaustReason, LineItems};

/// `fixed >= input_high` および必須費目の config 欠陥で停止するときの唯一のエラー名。
pub const CONTEXT_BUDGET_EXHAUSTED: &str = "context_budget_exhausted";

/// 文脈予算の解決・合成に失敗した。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextBudgetError {
    #[error("{0}")]
    MissingContextWindow(String),
    #[error("{0}")]
    MissingMaxOutputTokens(String),
    #[error("failed to look up model_pricing for \"{spec}\": {cause}")]
    LookupFailed { spec: String, cause: String },
    #[error("non-positive water inputs: window={window} output_reserve={output_reserve}")]
    NonPositiveWater {
        window: usize,
        output_reserve: usize,
    },
    #[error(
        "{CONTEXT_BUDGET_EXHAUSTED}: reason={reason:?} input_high={input_high} \
         mandatory_fixed={mandatory_fixed} fixed={fixed} system={system} \
         runtime_context={runtime_context} functions={functions} \
         output_reserve={output_reserve} memory_index={memory_index}"
    )]
    Exhausted {
        reason: BudgetExhaustReason,
        input_high: usize,
        input_low: usize,
        mandatory_fixed: usize,
        fixed: usize,
        system: usize,
        runtime_context: usize,
        functions: usize,
        output_reserve: usize,
        memory_index: usize,
    },
}

impl ContextBudgetError {
    /// 機械可読な一意名。超過停止は常に [`CONTEXT_BUDGET_EXHAUSTED`]。
    pub fn name(&self) -> &'static str {
        match self {
            Self::MissingContextWindow(_) => "model_context_window_missing",
            Self::MissingMaxOutputTokens(_) => "model_max_output_tokens_missing",
            Self::LookupFailed { .. } => "model_pricing_lookup_failed",
            Self::NonPositiveWater { .. } => "non_positive_water_inputs",
            Self::Exhausted { .. } => CONTEXT_BUDGET_EXHAUSTED,
        }
    }

    pub fn exhausted(
        reason: BudgetExhaustReason,
        water_input_high: usize,
        water_input_low: usize,
        items: &LineItems,
        mandatory_fixed: usize,
        fixed: usize,
    ) -> Self {
        Self::Exhausted {
            reason,
            input_high: water_input_high,
            input_low: water_input_low,
            mandatory_fixed,
            fixed,
            system: items.system,
            runtime_context: items.runtime_context,
            functions: items.functions,
            output_reserve: items.output_reserve,
            memory_index: items.memory_index,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_budget::envelope::LineItems;

    #[test]
    fn exhausted_name_is_unique_and_stable() {
        let items = LineItems {
            system: 1,
            runtime_context: 2,
            functions: 3,
            output_reserve: 4,
            memory_index: 5,
            conversation: 0,
        };
        let err = ContextBudgetError::exhausted(
            BudgetExhaustReason::MandatoryFixedExceedsInputHigh,
            10,
            5,
            &items,
            10,
            10,
        );
        assert_eq!(err.name(), CONTEXT_BUDGET_EXHAUSTED);
        assert!(err.to_string().starts_with(CONTEXT_BUDGET_EXHAUSTED));
    }
}
