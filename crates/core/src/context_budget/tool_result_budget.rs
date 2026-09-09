//! Tool resultを最終provider requestへ載せるための入力上限計算。
//!
//! 最大入力、最大出力、入力＋出力の共有上限を別の能力値として扱う。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInputLimits {
    pub max_input_tokens: Option<usize>,
    pub max_output_tokens: Option<usize>,
    pub max_total_tokens: Option<usize>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ToolResultBudgetError {
    #[error("model max_input_tokens is missing or non-positive")]
    InvalidMaxInput,
    #[error("model max_output_tokens is missing or non-positive")]
    InvalidMaxOutput,
    #[error("shared total token limit requires an explicit per-request output limit")]
    SharedLimitNeedsRequestedOutput,
    #[error("requested output tokens {requested} exceed model maximum {maximum}")]
    RequestedOutputExceedsMaximum { requested: usize, maximum: usize },
    #[error("shared total token limit {total} leaves no input after output reservation {requested_output}")]
    SharedLimitExhausted {
        total: usize,
        requested_output: usize,
    },
    #[error("base request tokens {base} exceed effective input limit {limit}")]
    BaseRequestExceedsInput { base: usize, limit: usize },
    #[error("measured request tokens {measured} exceed effective input limit {limit}")]
    FinalRequestExceedsInput { measured: usize, limit: usize },
    #[error("result page offset or cap is invalid")]
    InvalidPage,
}

pub fn effective_input_limit(
    limits: ModelInputLimits,
    requested_output_tokens: Option<usize>,
) -> Result<usize, ToolResultBudgetError> {
    let max_input = limits
        .max_input_tokens
        .filter(|value| *value > 0)
        .ok_or(ToolResultBudgetError::InvalidMaxInput)?;
    if let Some(requested) = requested_output_tokens {
        let max_output = limits
            .max_output_tokens
            .filter(|value| *value > 0)
            .ok_or(ToolResultBudgetError::InvalidMaxOutput)?;
        if requested == 0 || requested > max_output {
            return Err(ToolResultBudgetError::RequestedOutputExceedsMaximum {
                requested,
                maximum: max_output,
            });
        }
    }

    match (limits.max_total_tokens, requested_output_tokens) {
        (None, _) => Ok(max_input),
        (Some(_), None) => Err(ToolResultBudgetError::SharedLimitNeedsRequestedOutput),
        (Some(total), Some(requested)) if total > requested => Ok(max_input.min(total - requested)),
        (Some(total), Some(requested_output)) => Err(ToolResultBudgetError::SharedLimitExhausted {
            total,
            requested_output,
        }),
    }
}

pub fn available_result_tokens(
    limits: ModelInputLimits,
    requested_output_tokens: Option<usize>,
    base_request_tokens: usize,
) -> Result<usize, ToolResultBudgetError> {
    let limit = effective_input_limit(limits, requested_output_tokens)?;
    limit
        .checked_sub(base_request_tokens)
        .ok_or(ToolResultBudgetError::BaseRequestExceedsInput {
            base: base_request_tokens,
            limit,
        })
}

/// 完了順の各結果へ均等配分し、使われなかった分も同じ順序で再配分する。
pub fn allocate_result_tokens(available: usize, required: &[usize]) -> Vec<usize> {
    if required.is_empty() {
        return Vec::new();
    }
    let mut allocated = vec![0; required.len()];
    let initial = available / required.len();
    for (index, need) in required.iter().copied().enumerate() {
        allocated[index] = initial.min(need);
    }
    let mut remaining = available.saturating_sub(allocated.iter().sum::<usize>());

    while remaining > 0 {
        let active = required
            .iter()
            .zip(&allocated)
            .filter(|(need, have)| need > have)
            .count();
        if active == 0 {
            break;
        }
        let share = (remaining / active).max(1);
        let before = remaining;
        for (need, have) in required.iter().copied().zip(&mut allocated) {
            if remaining == 0 {
                break;
            }
            let grant = need.saturating_sub(*have).min(share).min(remaining);
            *have += grant;
            remaining -= grant;
        }
        if remaining == before {
            break;
        }
    }
    allocated
}

pub fn validate_final_request_tokens(
    measured_tokens: usize,
    effective_limit: usize,
) -> Result<(), ToolResultBudgetError> {
    if measured_tokens <= effective_limit {
        Ok(())
    } else {
        Err(ToolResultBudgetError::FinalRequestExceedsInput {
            measured: measured_tokens,
            limit: effective_limit,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultPage<'a> {
    pub body: &'a str,
    pub start_byte: usize,
    pub end_byte: usize,
    pub has_more: bool,
    pub next_byte: Option<usize>,
}

pub fn page_utf8_result(
    source: &str,
    start_byte: usize,
    max_bytes: usize,
) -> Result<ResultPage<'_>, ToolResultBudgetError> {
    if max_bytes == 0 || start_byte > source.len() || !source.is_char_boundary(start_byte) {
        return Err(ToolResultBudgetError::InvalidPage);
    }
    let mut end_byte = start_byte.saturating_add(max_bytes).min(source.len());
    while end_byte > start_byte && !source.is_char_boundary(end_byte) {
        end_byte -= 1;
    }
    if end_byte == start_byte && start_byte < source.len() {
        return Err(ToolResultBudgetError::InvalidPage);
    }
    let has_more = end_byte < source.len();
    Ok(ResultPage {
        body: &source[start_byte..end_byte],
        start_byte,
        end_byte,
        has_more,
        next_byte: has_more.then_some(end_byte),
    })
}
