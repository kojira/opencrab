//! 二水位・絶対上限 A・費目合成（#826-A）。
//!
//! ```text
//! input_high = min(floor(W * 0.50), A)
//! input_low  = min(floor(W * 0.25), floor(A / 2))
//! mandatory_fixed = system + runtime_context + functions + output_reserve
//! fixed = mandatory_fixed + injected_memory_index
//! conversation_high = input_high.saturating_sub(fixed)
//! conversation_low  = input_low.saturating_sub(fixed)
//! ```
//!
//! A の較正前初期値は 85–90K 劣化開始点より安全側の 80,000。chatgpt 305K 特例は置かない。

use super::error::ContextBudgetError;

/// 高水位の開始比（`W` に対する）。
pub const DEFAULT_INPUT_HIGH_RATIO: f64 = 0.50;
/// 低水位の開始比（`W` に対する）。
pub const DEFAULT_INPUT_LOW_RATIO: f64 = 0.25;
/// 較正前の絶対上限 A。約 85–90K token の劣化開始点より安全側。
pub const DEFAULT_ABSOLUTE_CAP_A: usize = 80_000;
/// Memory Index の較正前個別上限。会話の余りを暗黙に流用しない。
pub const DEFAULT_MEMORY_INDEX_TOKEN_CAP: usize = 4_000;
/// functions の較正前個別上限（実測 18,327–22,344 より安全側の頭）。
pub const DEFAULT_FUNCTIONS_TOKEN_CAP: usize = 24_000;

/// 水位と個別上限の政策値。
#[derive(Debug, Clone, PartialEq)]
pub struct ContextBudgetPolicy {
    pub input_high_ratio: f64,
    pub input_low_ratio: f64,
    pub absolute_cap_a: usize,
    pub memory_index_token_cap: usize,
    pub functions_token_cap: usize,
}

impl Default for ContextBudgetPolicy {
    fn default() -> Self {
        Self {
            input_high_ratio: DEFAULT_INPUT_HIGH_RATIO,
            input_low_ratio: DEFAULT_INPUT_LOW_RATIO,
            absolute_cap_a: DEFAULT_ABSOLUTE_CAP_A,
            memory_index_token_cap: DEFAULT_MEMORY_INDEX_TOKEN_CAP,
            functions_token_cap: DEFAULT_FUNCTIONS_TOKEN_CAP,
        }
    }
}

/// モデル窓から決まる水位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaterLevels {
    pub window: usize,
    pub absolute_cap_a: usize,
    pub input_high: usize,
    pub input_low: usize,
    pub output_reserve: usize,
}

/// 計測済み費目（注入判定前の生値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MeasuredLineItems {
    pub system: usize,
    pub runtime_context: usize,
    pub functions: usize,
    pub memory_index: usize,
    pub memory_index_entry_count: usize,
    pub conversation: usize,
}

/// 計上した費目。`memory_index` は注入後（省略なら 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineItems {
    pub system: usize,
    pub runtime_context: usize,
    pub functions: usize,
    pub output_reserve: usize,
    pub memory_index: usize,
    pub conversation: usize,
}

impl LineItems {
    /// 総 input（出力予約は含まない）。
    pub fn total_input(&self) -> usize {
        self.system
            .saturating_add(self.runtime_context)
            .saturating_add(self.functions)
            .saturating_add(self.memory_index)
            .saturating_add(self.conversation)
    }

    /// 総 input + 出力予約。
    pub fn total_with_reserve(&self) -> usize {
        self.total_input().saturating_add(self.output_reserve)
    }
}

/// Memory Index を丸ごと入れる / 丸ごと省略する。部分切り詰めは無い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryIndexDecision {
    Inject,
    Omit { reason: MemoryIndexOmitReason },
}

/// Memory Index を省略した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryIndexOmitReason {
    ExceedsDedicatedCap,
    ExceedsRemainingBudget,
}

/// 必須費目または合成後の超過理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetExhaustReason {
    MandatoryFixedExceedsInputHigh,
    FunctionsExceedCap,
}

/// 費目別予算の合成結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBudgetEnvelope {
    pub water: WaterLevels,
    pub items: LineItems,
    pub mandatory_fixed: usize,
    pub injected_memory_index: usize,
    pub memory_index_measured: usize,
    pub memory_index_entry_count: usize,
    pub memory_index_decision: MemoryIndexDecision,
    pub fixed: usize,
    pub conversation_high: usize,
    pub conversation_low: usize,
}

impl ContextBudgetEnvelope {
    /// `総 input + output_reserve`。
    pub fn accounted_total(&self) -> usize {
        self.items.total_with_reserve()
    }
}

fn floor_ratio(window: usize, ratio: f64) -> usize {
    ((window as f64) * ratio).floor() as usize
}

/// `W` と `A` と比から二水位を合成する。chatgpt 特例は持たない。
///
/// `window == 0` または `output_reserve == 0` は既定へ落とさず fail-loud。
pub fn compute_water_levels(
    window: usize,
    output_reserve: usize,
    policy: &ContextBudgetPolicy,
) -> Result<WaterLevels, ContextBudgetError> {
    if window == 0 || output_reserve == 0 {
        return Err(ContextBudgetError::NonPositiveWater {
            window,
            output_reserve,
        });
    }
    let input_high = floor_ratio(window, policy.input_high_ratio).min(policy.absolute_cap_a);
    let input_low = floor_ratio(window, policy.input_low_ratio).min(policy.absolute_cap_a / 2);
    Ok(WaterLevels {
        window,
        absolute_cap_a: policy.absolute_cap_a,
        input_high,
        input_low,
        output_reserve,
    })
}

/// Memory Index は専用 cap と残予算の双方に収まるときだけ全量注入する。
pub fn decide_memory_index(
    tokens: usize,
    cap: usize,
    remaining_after_mandatory: usize,
) -> MemoryIndexDecision {
    if tokens > cap {
        return MemoryIndexDecision::Omit {
            reason: MemoryIndexOmitReason::ExceedsDedicatedCap,
        };
    }
    // `fixed >= input_high` を空履歴で続行しない。残予算ちょうどの注入は会話を 0 にする。
    if tokens >= remaining_after_mandatory {
        return MemoryIndexDecision::Omit {
            reason: MemoryIndexOmitReason::ExceedsRemainingBudget,
        };
    }
    MemoryIndexDecision::Inject
}

/// 登録時・各 request 前の functions 上限検査。縮約しない。
pub fn ensure_functions_within_cap(tokens: usize, cap: usize) -> Result<(), ContextBudgetError> {
    if tokens > cap {
        let items = LineItems {
            functions: tokens,
            ..LineItems::default()
        };
        return Err(ContextBudgetError::exhausted(
            BudgetExhaustReason::FunctionsExceedCap,
            0,
            0,
            &items,
            tokens,
            tokens,
        ));
    }
    Ok(())
}

/// 必須費目を先に計上し、MI を全量注入または全量省略して envelope を返す。
pub fn apply_line_items(
    water: WaterLevels,
    measured: MeasuredLineItems,
    policy: &ContextBudgetPolicy,
) -> Result<ContextBudgetEnvelope, ContextBudgetError> {
    let items_for_err = LineItems {
        system: measured.system,
        runtime_context: measured.runtime_context,
        functions: measured.functions,
        output_reserve: water.output_reserve,
        memory_index: measured.memory_index,
        conversation: measured.conversation,
    };
    if measured.functions > policy.functions_token_cap {
        let mandatory_fixed = measured
            .system
            .saturating_add(measured.runtime_context)
            .saturating_add(measured.functions)
            .saturating_add(water.output_reserve);
        return Err(ContextBudgetError::exhausted(
            BudgetExhaustReason::FunctionsExceedCap,
            water.input_high,
            water.input_low,
            &items_for_err,
            mandatory_fixed,
            mandatory_fixed,
        ));
    }
    let mandatory_fixed = measured
        .system
        .saturating_add(measured.runtime_context)
        .saturating_add(measured.functions)
        .saturating_add(water.output_reserve);
    if mandatory_fixed >= water.input_high {
        return Err(ContextBudgetError::exhausted(
            BudgetExhaustReason::MandatoryFixedExceedsInputHigh,
            water.input_high,
            water.input_low,
            &items_for_err,
            mandatory_fixed,
            mandatory_fixed,
        ));
    }
    let remaining = water.input_high - mandatory_fixed;
    let memory_index_decision = decide_memory_index(
        measured.memory_index,
        policy.memory_index_token_cap,
        remaining,
    );
    let injected_memory_index = match memory_index_decision {
        MemoryIndexDecision::Inject => measured.memory_index,
        MemoryIndexDecision::Omit { .. } => 0,
    };
    let fixed = mandatory_fixed.saturating_add(injected_memory_index);
    if fixed >= water.input_high {
        return Err(ContextBudgetError::exhausted(
            BudgetExhaustReason::MandatoryFixedExceedsInputHigh,
            water.input_high,
            water.input_low,
            &items_for_err,
            mandatory_fixed,
            fixed,
        ));
    }
    let items = LineItems {
        system: measured.system,
        runtime_context: measured.runtime_context,
        functions: measured.functions,
        output_reserve: water.output_reserve,
        memory_index: injected_memory_index,
        conversation: measured.conversation,
    };
    Ok(ContextBudgetEnvelope {
        water,
        items,
        mandatory_fixed,
        injected_memory_index,
        memory_index_measured: measured.memory_index,
        memory_index_entry_count: measured.memory_index_entry_count,
        memory_index_decision,
        fixed,
        conversation_high: water.input_high.saturating_sub(fixed),
        conversation_low: water.input_low.saturating_sub(fixed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_budget::error::CONTEXT_BUDGET_EXHAUSTED;

    fn policy_with_a(a: usize) -> ContextBudgetPolicy {
        ContextBudgetPolicy {
            absolute_cap_a: a,
            ..ContextBudgetPolicy::default()
        }
    }

    #[test]
    fn water_levels_min_of_ratio_and_a() {
        let p = ContextBudgetPolicy::default();
        assert_eq!(p.absolute_cap_a, 80_000);
        assert_eq!(p.input_high_ratio, 0.50);
        assert_eq!(p.input_low_ratio, 0.25);

        // W=200_000: 比は 100_000 / 50_000、A が勝つ。
        let w = compute_water_levels(200_000, 4_096, &p).unwrap();
        assert_eq!(w.input_high, 80_000);
        assert_eq!(w.input_low, 40_000);
        assert_eq!(w.output_reserve, 4_096);

        // W=100_000: 比が A より小さい。
        let w = compute_water_levels(100_000, 4_096, &p).unwrap();
        assert_eq!(w.input_high, 50_000);
        assert_eq!(w.input_low, 25_000);
    }

    #[test]
    fn water_levels_boundaries_around_a() {
        let p = policy_with_a(80_000);
        // floor(160_000 * 0.50) = 80_000 == A
        let eq = compute_water_levels(160_000, 1, &p).unwrap();
        assert_eq!(eq.input_high, 80_000);
        assert_eq!(eq.input_low, 40_000);
        // floor(160_002 * 0.50) = 80_001 > A
        let over = compute_water_levels(160_002, 1, &p).unwrap();
        assert_eq!(over.input_high, 80_000);
        assert_eq!(over.input_low, 40_000);
        // floor(159_998 * 0.50) = 79_999 < A
        let under = compute_water_levels(159_998, 1, &p).unwrap();
        assert_eq!(under.input_high, 79_999);
        assert_eq!(under.input_low, 39_999);
    }

    #[test]
    fn water_levels_floor_fraction() {
        let p = policy_with_a(80_000);
        // floor(5 * 0.50) = 2, floor(5 * 0.25) = 1
        let w = compute_water_levels(5, 1, &p).unwrap();
        assert_eq!(w.input_high, 2);
        assert_eq!(w.input_low, 1);
    }

    #[test]
    fn water_levels_reject_zero_window_or_reserve() {
        let p = ContextBudgetPolicy::default();
        let err = compute_water_levels(0, 4_096, &p).unwrap_err();
        assert!(matches!(
            err,
            ContextBudgetError::NonPositiveWater {
                window: 0,
                output_reserve: 4_096
            }
        ));
        let err = compute_water_levels(100_000, 0, &p).unwrap_err();
        assert!(matches!(
            err,
            ContextBudgetError::NonPositiveWater {
                window: 100_000,
                output_reserve: 0
            }
        ));
    }

    #[test]
    fn chatgpt_catalog_window_uses_a_not_305k() {
        let p = ContextBudgetPolicy::default();
        let w = compute_water_levels(1_050_000, 4_096, &p).unwrap();
        assert_eq!(w.input_high, DEFAULT_ABSOLUTE_CAP_A);
        assert_ne!(w.input_high, 305_000);
        assert!(w.input_high < 85_000);
    }

    #[test]
    fn apply_injects_memory_index_when_under_both_caps() {
        let water = compute_water_levels(200_000, 4_000, &ContextBudgetPolicy::default()).unwrap();
        let env = apply_line_items(
            water,
            MeasuredLineItems {
                system: 1_000,
                runtime_context: 200,
                functions: 3_000,
                memory_index: 500,
                memory_index_entry_count: 12,
                conversation: 10_000,
            },
            &ContextBudgetPolicy::default(),
        )
        .unwrap();
        assert_eq!(env.memory_index_decision, MemoryIndexDecision::Inject);
        assert_eq!(env.injected_memory_index, 500);
        assert_eq!(env.mandatory_fixed, 1_000 + 200 + 3_000 + 4_000);
        assert_eq!(env.fixed, env.mandatory_fixed + 500);
        assert_eq!(env.conversation_high, water.input_high - env.fixed);
        assert_eq!(env.conversation_low, water.input_low - env.fixed);
        assert_eq!(
            env.accounted_total(),
            env.items.system
                + env.items.runtime_context
                + env.items.functions
                + env.items.memory_index
                + env.items.conversation
                + env.items.output_reserve
        );
    }

    #[test]
    fn memory_index_omitted_entirely_when_over_dedicated_cap() {
        let water = compute_water_levels(200_000, 1_000, &ContextBudgetPolicy::default()).unwrap();
        let env = apply_line_items(
            water,
            MeasuredLineItems {
                system: 100,
                runtime_context: 50,
                functions: 100,
                memory_index: DEFAULT_MEMORY_INDEX_TOKEN_CAP + 1,
                memory_index_entry_count: 40,
                conversation: 1_000,
            },
            &ContextBudgetPolicy::default(),
        )
        .unwrap();
        assert_eq!(
            env.memory_index_decision,
            MemoryIndexDecision::Omit {
                reason: MemoryIndexOmitReason::ExceedsDedicatedCap
            }
        );
        assert_eq!(env.injected_memory_index, 0);
        assert_eq!(env.items.memory_index, 0);
        assert_eq!(env.memory_index_entry_count, 40);
    }

    #[test]
    fn memory_index_omitted_entirely_when_over_remaining() {
        let policy = ContextBudgetPolicy {
            absolute_cap_a: 1_000,
            memory_index_token_cap: 4_000,
            ..ContextBudgetPolicy::default()
        };
        let water = compute_water_levels(10_000, 100, &policy).unwrap();
        // input_high = min(5000, 1000) = 1000
        // mandatory = 200+50+100+100 = 450, remaining = 550
        let env = apply_line_items(
            water,
            MeasuredLineItems {
                system: 200,
                runtime_context: 50,
                functions: 100,
                memory_index: 550,
                memory_index_entry_count: 3,
                conversation: 10,
            },
            &policy,
        )
        .unwrap();
        assert_eq!(
            env.memory_index_decision,
            MemoryIndexDecision::Omit {
                reason: MemoryIndexOmitReason::ExceedsRemainingBudget
            }
        );
        assert_eq!(env.injected_memory_index, 0);
        assert_eq!(
            env.conversation_high,
            water.input_high - env.mandatory_fixed
        );
    }

    #[test]
    fn fixed_at_or_over_input_high_is_context_budget_exhausted() {
        let policy = ContextBudgetPolicy {
            absolute_cap_a: 1_000,
            ..ContextBudgetPolicy::default()
        };
        let water = compute_water_levels(10_000, 400, &policy).unwrap();
        // input_high = 1000, mandatory = 300+100+200+400 = 1000
        let err = apply_line_items(
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
        )
        .unwrap_err();
        assert_eq!(err.name(), CONTEXT_BUDGET_EXHAUSTED);
        match err {
            ContextBudgetError::Exhausted {
                reason,
                mandatory_fixed,
                input_high,
                ..
            } => {
                assert_eq!(reason, BudgetExhaustReason::MandatoryFixedExceedsInputHigh);
                assert_eq!(mandatory_fixed, 1_000);
                assert_eq!(input_high, 1_000);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn functions_over_cap_is_context_budget_exhausted() {
        let err = ensure_functions_within_cap(
            DEFAULT_FUNCTIONS_TOKEN_CAP + 1,
            DEFAULT_FUNCTIONS_TOKEN_CAP,
        )
        .unwrap_err();
        assert_eq!(err.name(), CONTEXT_BUDGET_EXHAUSTED);
        let water = compute_water_levels(200_000, 1_000, &ContextBudgetPolicy::default()).unwrap();
        let err = apply_line_items(
            water,
            MeasuredLineItems {
                system: 10,
                runtime_context: 10,
                functions: DEFAULT_FUNCTIONS_TOKEN_CAP + 1,
                memory_index: 0,
                memory_index_entry_count: 0,
                conversation: 0,
            },
            &ContextBudgetPolicy::default(),
        )
        .unwrap_err();
        assert_eq!(err.name(), CONTEXT_BUDGET_EXHAUSTED);
    }

    /// 費目合計は加減算で一致する（property: 256 ケース）。
    #[test]
    fn line_item_totals_match_property() {
        let mut seed = 0xC0FFEE_u64;
        let mut next = |mod_n: u64| -> usize {
            seed = seed.wrapping_mul(0x5851F42D4C957F2D).wrapping_add(1);
            (seed % mod_n) as usize
        };
        for _ in 0..256 {
            let policy = ContextBudgetPolicy {
                absolute_cap_a: 8_000 + next(8_000),
                memory_index_token_cap: 100 + next(2_000),
                functions_token_cap: 500 + next(4_000),
                ..ContextBudgetPolicy::default()
            };
            let window = 20_000 + next(200_000);
            let reserve = 1 + next(2_000);
            let water = compute_water_levels(window, reserve, &policy).unwrap();
            let measured = MeasuredLineItems {
                system: next(3_000),
                runtime_context: next(500),
                functions: next(policy.functions_token_cap as u64 + 200),
                memory_index: next(policy.memory_index_token_cap as u64 + 200),
                memory_index_entry_count: next(7),
                conversation: next(20_000),
            };
            let mandatory = measured
                .system
                .saturating_add(measured.runtime_context)
                .saturating_add(measured.functions)
                .saturating_add(reserve);
            match apply_line_items(water, measured, &policy) {
                Ok(env) => {
                    assert_eq!(
                        env.mandatory_fixed,
                        measured.system + measured.runtime_context + measured.functions + reserve
                    );
                    assert_eq!(env.fixed, env.mandatory_fixed + env.injected_memory_index);
                    assert_eq!(
                        env.conversation_high,
                        env.water.input_high.saturating_sub(env.fixed)
                    );
                    assert_eq!(
                        env.conversation_low,
                        env.water.input_low.saturating_sub(env.fixed)
                    );
                    assert_eq!(
                        env.accounted_total(),
                        env.items.system
                            + env.items.runtime_context
                            + env.items.functions
                            + env.items.memory_index
                            + env.items.conversation
                            + env.items.output_reserve
                    );
                    assert!(env.fixed < env.water.input_high);
                    match env.memory_index_decision {
                        MemoryIndexDecision::Inject => {
                            assert_eq!(env.injected_memory_index, measured.memory_index);
                            assert!(measured.memory_index <= policy.memory_index_token_cap);
                        }
                        MemoryIndexDecision::Omit { .. } => {
                            assert_eq!(env.injected_memory_index, 0);
                        }
                    }
                }
                Err(err) => {
                    assert_eq!(err.name(), CONTEXT_BUDGET_EXHAUSTED);
                    assert!(
                        measured.functions > policy.functions_token_cap
                            || mandatory >= water.input_high
                    );
                }
            }
        }
    }

    /// 826-C 用の劣化帯走査点を固定する。実 LLM は呼ばない。305K clamp は選ばれない。
    #[test]
    fn degradation_band_harness_selects_a_not_305k() {
        let policy = ContextBudgetPolicy::default();
        let mut points = Vec::new();
        for prompt in (60_000..=110_000).step_by(5_000) {
            points.push(prompt);
            let water = compute_water_levels(1_050_000, 4_096, &policy).unwrap();
            assert_eq!(water.input_high, DEFAULT_ABSOLUTE_CAP_A);
            assert_ne!(water.input_high, 305_000);
        }
        assert_eq!(points.len(), 11);
        assert_eq!(points[0], 60_000);
        assert_eq!(*points.last().unwrap(), 110_000);
    }
}
