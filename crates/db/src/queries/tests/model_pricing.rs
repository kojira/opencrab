use super::*;

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
