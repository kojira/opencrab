// 15. test_model_pricing_upsert_and_get
#[test]
fn test_model_pricing_upsert_and_get() {
    let conn = setup();

    let pricing = ModelPricingRow {
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        input_price_per_1m: 30.0,
        output_price_per_1m: 60.0,
        context_window: Some(128000),
        max_output_tokens: Some(8192),
        cached_input_price_per_1m: None,
        cache_write_price_per_1m: None,
    };

    upsert_model_pricing(&conn, &pricing).unwrap();

    let fetched = get_model_pricing(&conn, "openai", "gpt-4").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.provider, "openai");
    assert_eq!(fetched.model, "gpt-4");
    assert!((fetched.input_price_per_1m - 30.0).abs() < 1e-9);
    assert!((fetched.output_price_per_1m - 60.0).abs() < 1e-9);
    assert_eq!(fetched.context_window, Some(128000));
}

// v58: cache prices round-trip and are used by the pay-as-you-go cost.
#[test]
fn model_pricing_cache_prices_drive_cost() {
    let conn = setup();
    let row = ModelPricingRow {
        provider: "chatgpt".to_string(),
        model: "gpt-6.1-sol".to_string(),
        input_price_per_1m: 2.0,
        output_price_per_1m: 10.0,
        context_window: Some(400_000),
        max_output_tokens: None,
        cached_input_price_per_1m: Some(0.1),
        cache_write_price_per_1m: Some(2.5),
    };
    upsert_model_pricing(&conn, &row).unwrap();
    let fetched = get_model_pricing(&conn, "chatgpt", "gpt-6.1-sol")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.cached_input_price_per_1m, Some(0.1));
    assert_eq!(fetched.cache_write_price_per_1m, Some(2.5));

    // OpenAI-style: 1M prompt tokens of which 800K are cached reads.
    let included = fetched.cost_usd(BilledTokens {
        input_tokens: 1_000_000,
        output_tokens: 100_000,
        cache_read_tokens: 800_000,
        cache_write_tokens: 0,
        cache_included_in_input: true,
    });
    assert!((included - (0.2 * 2.0 + 0.8 * 0.1 + 0.1 * 10.0)).abs() < 1e-9, "{included}");

    // Anthropic-style: uncached input, reads and writes are separate counts.
    let separate = fetched.cost_usd(BilledTokens {
        input_tokens: 1_000,
        output_tokens: 0,
        cache_read_tokens: 1_000_000,
        cache_write_tokens: 100_000,
        cache_included_in_input: false,
    });
    assert!((separate - (0.001 * 2.0 + 0.1 + 0.1 * 2.5)).abs() < 1e-9, "{separate}");
}

// hermit-style: prompt count = uncached + cache read + cache write.
#[test]
fn included_cache_subtracts_reads_and_writes_from_input() {
    let row = ModelPricingRow {
        provider: "hermit".to_string(),
        model: "claude-opus-5-5".to_string(),
        input_price_per_1m: 4.0,
        output_price_per_1m: 20.0,
        context_window: None,
        max_output_tokens: None,
        cached_input_price_per_1m: Some(0.2),
        cache_write_price_per_1m: Some(5.0),
    };
    let cost = row.cost_usd(BilledTokens {
        input_tokens: 1_100_000,
        output_tokens: 0,
        cache_read_tokens: 900_000,
        cache_write_tokens: 100_000,
        cache_included_in_input: true,
    });
    assert!((cost - (0.1 * 4.0 + 0.9 * 0.2 + 0.1 * 5.0)).abs() < 1e-9, "{cost}");
}
