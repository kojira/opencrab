//! 文脈予算の envelope と観測基盤（#826-A）。
//!
//! `model_pricing.context_window` / `max_output_tokens` を必須値とし、欠落・NULL・0 は
//! 既定へ落とさず fail-loud する。水位は `min(floor(W * 比), A)`。chatgpt 305K 特例と
//! 100K 隠れフォールバックは置かない。

mod envelope;
mod error;
mod ledger;
mod observe;

pub use envelope::{
    apply_line_items, compute_water_levels, decide_memory_index, ensure_functions_within_cap,
    BudgetExhaustReason, ContextBudgetEnvelope, ContextBudgetPolicy, LineItems, MeasuredLineItems,
    MemoryIndexDecision, MemoryIndexOmitReason, WaterLevels, DEFAULT_ABSOLUTE_CAP_A,
    DEFAULT_FUNCTIONS_TOKEN_CAP, DEFAULT_INPUT_HIGH_RATIO, DEFAULT_INPUT_LOW_RATIO,
    DEFAULT_MEMORY_INDEX_TOKEN_CAP,
};
pub use error::{ContextBudgetError, CONTEXT_BUDGET_EXHAUSTED};
pub use ledger::{LedgerItem, TokenLedger};
pub use observe::{
    emit_context_budget_check, exhausted_check, BudgetCheckAction, ContextBudgetCheck,
};

/// `provider:model` 形式（またはモデル名のみ）を pricing 参照用に分割する。
pub fn split_llm_model_spec(full: &str) -> (&str, &str) {
    if let Some(i) = full.find(':') {
        (&full[..i], &full[i + 1..])
    } else {
        ("", full)
    }
}

/// 未登録モデルを設定しようとしたときのエラーメッセージ（#412）。
///
/// **登録方法まで書く。** 拒否だけして先へ進む手段を示さないと、「設定できないが
/// どうすれば設定できるかも分からない」で止まる。
///
/// **フロントの導線がこの文言に依存している（#482）。** `web/src/pages/AgentOverview.tsx`
/// の `UNREGISTERED_MARKER`（= `"has no context_window registered in model_pricing"`）と
/// 正規表現 `/model "([^"]+)"/` が、このメッセージから「未登録である」ことと spec を
/// 拾って、その場に登録フォームを出す。**この文言を変えるなら AgentOverview.tsx も
/// 直せ。** さもないと導線が黙って出なくなり、運用者は curl 直叩きに戻る。
/// 契約は `missing_message_keeps_frontend_link_contract` テストで固定している。
pub fn model_context_window_missing_message(spec: &str) -> String {
    format!(
        "model \"{spec}\" has no context_window registered in model_pricing. \
         Register it first: PUT /api/llm/model-pricing with body \
         {{\"provider\": \"...\", \"model\": \"...\", \"input_price_per_1m\": 0.0, \
         \"output_price_per_1m\": 0.0, \"context_window\": <max tokens>}}. \
         Current registrations: GET /api/llm/model-pricing."
    )
}

/// `model_pricing` を引くときのキー。**投入 API の保存キーと同じ正規化**（両端の
/// 空白を落とす）を掛ける。
fn model_pricing_key(provider: &str, model: &str) -> (String, String) {
    (provider.trim().to_string(), model.trim().to_string())
}

/// `provider:model` 形式の spec を比較用に正規化する（#412）。
pub fn normalize_model_spec(spec: &str) -> String {
    let (provider, model) = split_llm_model_spec(spec);
    let (provider, model) = model_pricing_key(provider, model);
    format!("{provider}:{model}")
}

/// 文脈予算に使える `context_window` か（#412）。`None` / 0 以下は未登録。
fn usable_context_window(row: &opencrab_db::queries::ModelPricingRow) -> Option<i32> {
    row.context_window.filter(|w| *w > 0)
}

/// `provider:model` 形式の spec が `model_pricing` に `context_window` を持つ行を
/// 持つか検証する（#412）。
pub fn ensure_model_context_window_registered(
    conn: &rusqlite::Connection,
    spec: &str,
) -> Result<(), String> {
    let (provider, model) = split_llm_model_spec(spec);
    let (provider, model) = model_pricing_key(provider, model);
    match opencrab_db::queries::get_model_pricing(conn, &provider, &model) {
        Ok(Some(p)) if usable_context_window(&p).is_some() => Ok(()),
        Ok(_) => Err(model_context_window_missing_message(spec)),
        Err(e) => Err(format!(
            "failed to look up model_pricing for \"{spec}\": {e}"
        )),
    }
}

/// #676: モデルの `max_output_tokens` 未登録メッセージ。登録先を案内する。
pub fn model_max_output_tokens_missing_message(spec: &str) -> String {
    format!(
        "model \"{spec}\" has no max_output_tokens registered in model_pricing. \
         Register it first: PUT /api/llm/model-pricing with body \
         {{\"provider\": \"...\", \"model\": \"...\", \"context_window\": <max tokens>, \
         \"max_output_tokens\": <model's output cap>}}. \
         Current registrations: GET /api/llm/model-pricing."
    )
}

/// 使える `max_output_tokens` か。`None` / 0 以下は「未登録」扱い。
fn usable_max_output_tokens(row: &opencrab_db::queries::ModelPricingRow) -> Option<u32> {
    row.max_output_tokens.filter(|w| *w > 0).map(|w| w as u32)
}

/// 使用モデルの `max_output_tokens` を `model_pricing` から解決する。無 / NULL / 0 は
/// fail loud（既定値へ落とさない）。
pub fn resolve_model_max_output_tokens(
    conn: &rusqlite::Connection,
    spec: &str,
) -> Result<u32, String> {
    let (provider, model) = split_llm_model_spec(spec);
    let (provider, model) = model_pricing_key(provider, model);
    match opencrab_db::queries::get_model_pricing(conn, &provider, &model) {
        Ok(Some(p)) => usable_max_output_tokens(&p)
            .ok_or_else(|| model_max_output_tokens_missing_message(spec)),
        Ok(None) => Err(model_max_output_tokens_missing_message(spec)),
        Err(e) => Err(format!(
            "failed to look up model_pricing for \"{spec}\": {e}"
        )),
    }
}

/// `max_output_tokens` を持つ行があるか検証する。
pub fn ensure_model_max_output_tokens_registered(
    conn: &rusqlite::Connection,
    spec: &str,
) -> Result<(), String> {
    resolve_model_max_output_tokens(conn, spec).map(|_| ())
}

/// 窓と出力予約を同時に解決する。どちらかが無 / NULL / 0 なら既定へ落とさない。
pub fn resolve_model_budget_inputs(
    conn: &rusqlite::Connection,
    provider: &str,
    model: &str,
) -> Result<(usize, usize), ContextBudgetError> {
    let spec = {
        let (p, m) = model_pricing_key(provider, model);
        if p.is_empty() {
            m
        } else {
            format!("{p}:{m}")
        }
    };
    let (provider, model) = model_pricing_key(provider, model);
    match opencrab_db::queries::get_model_pricing(conn, &provider, &model) {
        Ok(Some(p)) => {
            let window = usable_context_window(&p)
                .map(|w| w as usize)
                .ok_or_else(|| {
                    ContextBudgetError::MissingContextWindow(model_context_window_missing_message(
                        &spec,
                    ))
                })?;
            let reserve = usable_max_output_tokens(&p)
                .map(|w| w as usize)
                .ok_or_else(|| {
                    ContextBudgetError::MissingMaxOutputTokens(
                        model_max_output_tokens_missing_message(&spec),
                    )
                })?;
            Ok((window, reserve))
        }
        Ok(None) => Err(ContextBudgetError::MissingContextWindow(
            model_context_window_missing_message(&spec),
        )),
        Err(e) => Err(ContextBudgetError::LookupFailed {
            spec,
            cause: e.to_string(),
        }),
    }
}

/// 起動時: 既定モデルと全エージェントの実効モデルが窓・出力予約を持つこと。
pub fn ensure_startup_budget_inputs(
    conn: &rusqlite::Connection,
    default_spec: &str,
) -> Result<(), ContextBudgetError> {
    let (provider, model) = split_llm_model_spec(default_spec);
    resolve_model_budget_inputs(conn, provider, model)?;
    let ids = opencrab_db::queries::list_agent_ids(conn).map_err(|e| {
        ContextBudgetError::LookupFailed {
            spec: default_spec.to_string(),
            cause: e.to_string(),
        }
    })?;
    for agent_id in ids {
        let spec = opencrab_db::queries::effective_model_for_agent(conn, &agent_id, default_spec)
            .map_err(|e| ContextBudgetError::LookupFailed {
            spec: default_spec.to_string(),
            cause: e.to_string(),
        })?;
        let (provider, model) = split_llm_model_spec(&spec);
        resolve_model_budget_inputs(conn, provider, model)?;
    }
    Ok(())
}

/// エージェントのモデルを**新しく設定するとき**だけ、`model_pricing` の登録を
/// 要求する（#412 / #826-A）。
///
/// `context_window` と `max_output_tokens` の両方を要求する（出力予約が必須のため）。
/// `_provider_sends_max_output_tokens` は呼び出し側互換のために残すが、826-A では
/// 送らないプロバイダでも予約値は必須なので見ない。
pub fn check_agent_model_change(
    conn: &rusqlite::Connection,
    existing: Option<&opencrab_db::queries::AgentRow>,
    new_model: Option<&str>,
    _provider_sends_max_output_tokens: bool,
) -> Result<(), String> {
    let Some(new_model) = new_model.filter(|m| !m.is_empty()) else {
        return Ok(());
    };
    if existing.and_then(|a| a.model.as_deref()) == Some(new_model) {
        return Ok(());
    }
    ensure_model_context_window_registered(conn, new_model)?;
    ensure_model_max_output_tokens_registered(conn, new_model)?;
    Ok(())
}

/// 水位を DB から解決する。窓または予約が無ければ fail-loud。
pub fn resolve_water_levels(
    conn: &rusqlite::Connection,
    provider: &str,
    model: &str,
    policy: &ContextBudgetPolicy,
) -> Result<WaterLevels, ContextBudgetError> {
    let (window, output_reserve) = resolve_model_budget_inputs(conn, provider, model)?;
    Ok(compute_water_levels(window, output_reserve, policy))
}

/// 呼び出し元が使う `input_high`（`min(floor(W * ratio), A)`）。
///
/// 行が引けない / 窓が 0 / 予約が無ければ既定へ落とさず `Err`。
pub fn compute_context_budget(
    conn: &rusqlite::Connection,
    provider: &str,
    model: &str,
    compaction_ratio: f64,
) -> Result<usize, ContextBudgetError> {
    let policy = ContextBudgetPolicy {
        input_high_ratio: compaction_ratio,
        ..ContextBudgetPolicy::default()
    };
    Ok(resolve_water_levels(conn, provider, model, &policy)?.input_high)
}

/// 未登録モデルを設定時に弾き、実行時も既定へ落とさない（#412 / #826-A）。
#[cfg(test)]
mod model_context_window_gate_tests {
    use super::{
        check_agent_model_change, compute_context_budget, ensure_model_context_window_registered,
        ensure_startup_budget_inputs, resolve_model_budget_inputs, ContextBudgetError,
        DEFAULT_ABSOLUTE_CAP_A,
    };

    fn register(conn: &rusqlite::Connection, provider: &str, model: &str, window: Option<i32>) {
        register_full(conn, provider, model, window, Some(4_096));
    }

    fn register_full(
        conn: &rusqlite::Connection,
        provider: &str,
        model: &str,
        window: Option<i32>,
        max_output: Option<i32>,
    ) {
        opencrab_db::queries::upsert_model_pricing(
            conn,
            &opencrab_db::queries::ModelPricingRow {
                provider: provider.to_string(),
                model: model.to_string(),
                input_price_per_1m: 0.0,
                output_price_per_1m: 0.0,
                context_window: window,
                max_output_tokens: max_output,
            },
        )
        .unwrap();
    }

    #[test]
    fn registered_model_passes() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "p1", "m1", Some(200_000));
        assert!(ensure_model_context_window_registered(&conn, "p1:m1").is_ok());
    }

    #[test]
    fn unregistered_model_is_rejected_with_how_to_register() {
        let conn = opencrab_db::init_memory().unwrap();
        let err = ensure_model_context_window_registered(&conn, "p1:m1").unwrap_err();
        assert!(err.contains("model_pricing"), "{err}");
        assert!(err.contains("/api/llm/model-pricing"), "{err}");
    }

    #[test]
    fn missing_message_keeps_frontend_link_contract() {
        let msg = super::model_context_window_missing_message("chatgpt:gpt-5.6-terra");
        assert!(
            msg.contains("has no context_window registered in model_pricing"),
            "{msg}"
        );
        assert!(msg.contains("model \"chatgpt:gpt-5.6-terra\""), "{msg}");
    }

    #[test]
    fn row_without_context_window_is_rejected() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "p1", "m1", None);
        assert!(ensure_model_context_window_registered(&conn, "p1:m1").is_err());
    }

    #[test]
    fn bare_model_spec_uses_empty_provider() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "", "m1", Some(123));
        assert!(ensure_model_context_window_registered(&conn, "m1").is_ok());
        assert!(ensure_model_context_window_registered(&conn, "p1:m1").is_err());
    }

    #[test]
    fn non_positive_context_window_is_treated_as_unregistered() {
        for bad in [0, -1, -200_000] {
            let conn = opencrab_db::init_memory().unwrap();
            register(&conn, "p1", "m1", Some(bad));
            assert!(
                ensure_model_context_window_registered(&conn, "p1:m1").is_err(),
                "context_window={bad} は未登録扱いのはず"
            );
            let err = compute_context_budget(&conn, "p1", "m1", 0.5).unwrap_err();
            assert!(
                matches!(err, ContextBudgetError::MissingContextWindow(_)),
                "context_window={bad} は fail-loud: {err}"
            );
        }
    }

    #[test]
    fn spec_normalization_ignores_surrounding_whitespace() {
        use super::normalize_model_spec as norm;
        assert_eq!(norm(" p1 : m1 "), norm("p1:m1"));
        assert_eq!(norm("p1:m1\n"), norm("p1:m1"));
        assert_ne!(norm("p1:m1"), norm("p1:m2"));
        assert_eq!(norm(" p1 : a/b:c "), "p1:a/b:c");
    }

    #[test]
    fn lookup_ignores_surrounding_whitespace() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "p1", "m1", Some(200_000));
        assert!(ensure_model_context_window_registered(&conn, " p1 : m1 ").is_ok());
        assert_eq!(
            compute_context_budget(&conn, " p1 ", " m1 ", 0.5).unwrap(),
            DEFAULT_ABSOLUTE_CAP_A
        );
    }

    #[test]
    fn budget_uses_registered_context_window_capped_by_a() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "p1", "m1", Some(200_000));
        // 200_000 × 0.5 = 100_000、A=80_000 が勝つ。
        assert_eq!(
            compute_context_budget(&conn, "p1", "m1", 0.5).unwrap(),
            DEFAULT_ABSOLUTE_CAP_A
        );
    }

    #[test]
    fn budget_fails_loud_when_unregistered() {
        let conn = opencrab_db::init_memory().unwrap();
        let err = compute_context_budget(&conn, "p1", "m1", 0.5).unwrap_err();
        assert!(matches!(err, ContextBudgetError::MissingContextWindow(_)));
    }

    #[test]
    fn missing_or_zero_output_reserve_fails_loud() {
        let conn = opencrab_db::init_memory().unwrap();
        register_full(&conn, "p1", "m1", Some(200_000), None);
        let err = resolve_model_budget_inputs(&conn, "p1", "m1").unwrap_err();
        assert!(matches!(err, ContextBudgetError::MissingMaxOutputTokens(_)));

        let conn = opencrab_db::init_memory().unwrap();
        register_full(&conn, "p1", "m1", Some(200_000), Some(0));
        let err = compute_context_budget(&conn, "p1", "m1", 0.5).unwrap_err();
        assert!(matches!(err, ContextBudgetError::MissingMaxOutputTokens(_)));
    }

    #[test]
    fn chatgpt_catalog_window_uses_a_not_305k_clamp() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "chatgpt", "gpt-5.6-sol", Some(1_050_000));
        assert_eq!(
            compute_context_budget(&conn, "chatgpt", "gpt-5.6-sol", 0.5).unwrap(),
            DEFAULT_ABSOLUTE_CAP_A
        );
        assert_ne!(
            compute_context_budget(&conn, "chatgpt", "gpt-5.6-sol", 0.5).unwrap(),
            305_000
        );
    }

    #[test]
    fn provider_without_old_ceiling_is_also_capped_by_a() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "p1", "m1", Some(1_050_000));
        assert_eq!(
            compute_context_budget(&conn, "p1", "m1", 0.5).unwrap(),
            DEFAULT_ABSOLUTE_CAP_A
        );
    }

    #[test]
    fn window_below_a_uses_ratio() {
        let conn = opencrab_db::init_memory().unwrap();
        register(&conn, "chatgpt", "gpt-5.6-sol", Some(100_000));
        assert_eq!(
            compute_context_budget(&conn, "chatgpt", "gpt-5.6-sol", 0.5).unwrap(),
            50_000
        );
    }

    #[test]
    fn startup_requires_window_and_reserve_for_default_and_agents() {
        let conn = opencrab_db::init_memory().unwrap();
        assert!(ensure_startup_budget_inputs(&conn, "p1:m1").is_err());
        register(&conn, "p1", "m1", Some(200_000));
        assert!(ensure_startup_budget_inputs(&conn, "p1:m1").is_ok());
    }

    #[test]
    fn resolve_max_output_returns_registered_value() {
        let conn = opencrab_db::init_memory().unwrap();
        register_full(
            &conn,
            "hermit",
            "claude-opus-5",
            Some(1_000_000),
            Some(128_000),
        );
        assert_eq!(
            super::resolve_model_max_output_tokens(&conn, "hermit:claude-opus-5"),
            Ok(128_000)
        );
    }

    #[test]
    fn resolve_max_output_missing_is_err_with_registration_hint() {
        let conn = opencrab_db::init_memory().unwrap();
        let err = super::resolve_model_max_output_tokens(&conn, "p1:m1").unwrap_err();
        assert!(err.contains("max_output_tokens"), "{err}");
        assert!(err.contains("/api/llm/model-pricing"), "{err}");
        register_full(&conn, "p1", "m1", Some(200_000), None);
        assert!(super::resolve_model_max_output_tokens(&conn, "p1:m1").is_err());
    }

    #[test]
    fn resolve_max_output_non_positive_is_unregistered() {
        for bad in [0, -1, -128_000] {
            let conn = opencrab_db::init_memory().unwrap();
            register_full(&conn, "p1", "m1", Some(200_000), Some(bad));
            assert!(
                super::resolve_model_max_output_tokens(&conn, "p1:m1").is_err(),
                "max_output_tokens={bad} は未登録扱いのはず"
            );
        }
    }

    #[test]
    fn model_change_gate_requires_max_output_always() {
        let conn = opencrab_db::init_memory().unwrap();
        register_full(&conn, "hermit", "claude-opus-5", Some(1_000_000), None);
        let err =
            check_agent_model_change(&conn, None, Some("hermit:claude-opus-5"), true).unwrap_err();
        assert!(err.contains("max_output_tokens"), "{err}");

        register_full(
            &conn,
            "hermit",
            "claude-opus-5",
            Some(1_000_000),
            Some(128_000),
        );
        assert!(check_agent_model_change(&conn, None, Some("hermit:claude-opus-5"), true).is_ok());
    }

    #[test]
    fn model_change_gate_requires_max_output_even_when_provider_does_not_send() {
        let conn = opencrab_db::init_memory().unwrap();
        register_full(&conn, "chatgpt", "gpt-5.6-sol", Some(350_000), None);
        let err =
            check_agent_model_change(&conn, None, Some("chatgpt:gpt-5.6-sol"), false).unwrap_err();
        assert!(
            err.contains("max_output_tokens"),
            "出力予約は送らないプロバイダでも必須: {err}"
        );
        assert!(check_agent_model_change(&conn, None, Some("chatgpt:unknown"), false).is_err());
    }
}
