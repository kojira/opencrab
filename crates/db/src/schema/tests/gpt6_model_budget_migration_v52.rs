#[test]
fn v51_to_v52_seeds_gpt_6_chatgpt_model_budgets_without_overwriting_operator_rows() {
    let conn = Connection::open_in_memory().unwrap();
    initialize(&conn).unwrap();

    conn.execute_batch(
        "DELETE FROM model_pricing WHERE provider = 'chatgpt' AND model IN ('gpt-6-sol', 'gpt-6-luna');
         INSERT INTO model_pricing (
             provider, model, input_price_per_1m, output_price_per_1m,
             context_window, max_output_tokens, updated_at
         ) VALUES ('chatgpt', 'gpt-6-sol', 7.0, 9.0, 123456, 6543, 'operator');
         PRAGMA user_version = 51;",
    )
    .unwrap();

    initialize(&conn).unwrap();

    let sol = crate::queries::get_model_pricing(&conn, "chatgpt", "gpt-6-sol")
        .unwrap()
        .unwrap();
    assert_eq!(sol.input_price_per_1m, 7.0);
    assert_eq!(sol.output_price_per_1m, 9.0);
    assert_eq!(sol.context_window, Some(123456));
    assert_eq!(sol.max_output_tokens, Some(6543));

    let luna = crate::queries::get_model_pricing(&conn, "chatgpt", "gpt-6-luna")
        .unwrap()
        .unwrap();
    assert_eq!(luna.input_price_per_1m, 0.0);
    assert_eq!(luna.output_price_per_1m, 0.0);
    assert_eq!(luna.context_window, Some(400000));
    assert_eq!(luna.max_output_tokens, Some(32000));
    assert_eq!(schema_version(&conn).unwrap(), 52);
}
