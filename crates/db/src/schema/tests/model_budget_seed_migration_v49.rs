#[test]
fn v48_to_v49_completes_existing_default_and_only_fills_known_null_budgets() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE model_pricing (
            provider TEXT NOT NULL,
            model TEXT NOT NULL,
            input_price_per_1m REAL NOT NULL,
            output_price_per_1m REAL NOT NULL,
            context_window INTEGER,
            max_output_tokens INTEGER,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (provider, model)
         );
         INSERT INTO model_pricing VALUES
            ('codex', 'gpt-5.6', 9.0, 10.0, NULL, NULL, 'operator-default'),
            ('chatgpt', 'gpt-5.6-sol', 5.0, 30.0, 350000, NULL, 'operator-sol'),
            ('chatgpt', 'gpt-5.6-terra', 2.0, 12.0, 350000, 12345, 'operator-terra'),
            ('cursor', 'cursor-grok-4.6-high', 2.0, 6.0, 100000, NULL, 'operator-cursor'),
            ('custom', 'unknown', 7.0, 8.0, 90000, NULL, 'operator-custom');
         PRAGMA user_version = 48;",
    )
    .unwrap();

    initialize(&conn).unwrap();
    initialize(&conn).unwrap();

    assert_eq!(schema_version(&conn).unwrap(), 49);
    let default = crate::queries::get_model_pricing(&conn, "codex", "gpt-5.6")
        .unwrap()
        .expect("existing default row");
    assert_eq!(default.input_price_per_1m, 9.0);
    assert_eq!(default.output_price_per_1m, 10.0);
    assert_eq!(default.context_window, Some(1_050_000));
    assert_eq!(default.max_output_tokens, Some(32_000));

    let sol = crate::queries::get_model_pricing(&conn, "chatgpt", "gpt-5.6-sol")
        .unwrap()
        .unwrap();
    assert_eq!(sol.input_price_per_1m, 5.0);
    assert_eq!(sol.output_price_per_1m, 30.0);
    assert_eq!(sol.context_window, Some(350_000));
    assert_eq!(sol.max_output_tokens, Some(32_000));

    let terra = crate::queries::get_model_pricing(&conn, "chatgpt", "gpt-5.6-terra")
        .unwrap()
        .unwrap();
    assert_eq!(terra.max_output_tokens, Some(12_345));
    let cursor =
        crate::queries::get_model_pricing(&conn, "cursor", "cursor-grok-4.6-high")
            .unwrap()
            .unwrap();
    assert_eq!(cursor.max_output_tokens, Some(32_000));
    let unknown = crate::queries::get_model_pricing(&conn, "custom", "unknown")
        .unwrap()
        .unwrap();
    assert_eq!(unknown.max_output_tokens, None);
}

#[test]
fn fresh_schema_seeds_complete_standard_default_budget() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();

    assert_eq!(schema_version(&conn).unwrap(), 49);
    let default = crate::queries::get_model_pricing(&conn, "codex", "gpt-5.6")
        .unwrap()
        .expect("standard default pricing row");
    assert_eq!(default.context_window, Some(1_050_000));
    assert_eq!(default.max_output_tokens, Some(32_000));
}
