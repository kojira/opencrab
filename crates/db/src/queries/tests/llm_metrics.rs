use super::*;

// 13. test_llm_metrics_insert_and_summary
#[test]
fn test_llm_metrics_insert_and_summary() {
    let conn = setup();

    let metrics1 = LlmMetricsRow {
        id: "metrics-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("session-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        purpose: "discussion".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("medium".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 1200,
        time_to_first_token_ms: Some(200),
    };

    let metrics2 = LlmMetricsRow {
        id: "metrics-2".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("session-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        purpose: "summarization".to_string(),
        task_type: Some("summary".to_string()),
        complexity: Some("low".to_string()),
        input_tokens: 200,
        output_tokens: 80,
        total_tokens: 280,
        estimated_cost_usd: 0.008,
        latency_ms: 800,
        time_to_first_token_ms: Some(150),
    };

    insert_llm_metrics(&conn, &metrics1).unwrap();
    insert_llm_metrics(&conn, &metrics2).unwrap();

    let summary = get_llm_metrics_summary(&conn, "agent-1", "2020-01-01").unwrap();
    assert_eq!(summary.count, 2);
    assert_eq!(summary.total_tokens, Some(430));
    let total_cost = summary.total_cost.unwrap();
    assert!((total_cost - 0.013).abs() < 1e-9);
    let avg_latency = summary.avg_latency.unwrap();
    assert!((avg_latency - 1000.0).abs() < 1e-9);
}

// 14. test_llm_metrics_evaluation_update
#[test]
fn test_llm_metrics_evaluation_update() {
    let conn = setup();

    let metrics = LlmMetricsRow {
        id: "metrics-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("session-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4".to_string(),
        purpose: "discussion".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("medium".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 1200,
        time_to_first_token_ms: Some(200),
    };

    insert_llm_metrics(&conn, &metrics).unwrap();
    update_llm_metrics_evaluation(&conn, "metrics-1", 0.95, true, "excellent response").unwrap();

    // Read back via raw SQL to verify the evaluation columns
    let (quality_score, task_success, self_evaluation): (f64, i32, String) = conn
        .query_row(
            "SELECT quality_score, task_success, self_evaluation FROM llm_usage_metrics WHERE id = ?1",
            params!["metrics-1"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert!((quality_score - 0.95).abs() < 1e-9);
    assert_eq!(task_success, 1);
    assert_eq!(self_evaluation, "excellent response");
}

// 14b. test_llm_metrics_by_model
#[test]
fn test_llm_metrics_by_model() {
    let conn = setup();

    let m1 = LlmMetricsRow {
        id: "m-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        purpose: "conversation".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("medium".to_string()),
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 1200,
        time_to_first_token_ms: Some(200),
    };
    let m2 = LlmMetricsRow {
        id: "m-2".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "conversation".to_string(),
        task_type: Some("chat".to_string()),
        complexity: Some("low".to_string()),
        input_tokens: 80,
        output_tokens: 40,
        total_tokens: 120,
        estimated_cost_usd: 0.001,
        latency_ms: 400,
        time_to_first_token_ms: Some(100),
    };
    let m3 = LlmMetricsRow {
        id: "m-3".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "analysis".to_string(),
        task_type: Some("summary".to_string()),
        complexity: Some("low".to_string()),
        input_tokens: 60,
        output_tokens: 30,
        total_tokens: 90,
        estimated_cost_usd: 0.0008,
        latency_ms: 300,
        time_to_first_token_ms: Some(80),
    };

    insert_llm_metrics(&conn, &m1).unwrap();
    insert_llm_metrics(&conn, &m2).unwrap();
    insert_llm_metrics(&conn, &m3).unwrap();

    let stats = get_llm_metrics_by_model(&conn, "agent-1", "2020-01-01").unwrap();
    assert_eq!(stats.len(), 2);

    // gpt-4o-mini has 2 records, gpt-4o has 1 → sorted by count DESC
    assert_eq!(stats[0].model, "gpt-4o-mini");
    assert_eq!(stats[0].count, 2);
    assert_eq!(stats[0].total_tokens, 210);
    assert!((stats[0].total_cost - 0.0018).abs() < 1e-9);

    assert_eq!(stats[1].model, "gpt-4o");
    assert_eq!(stats[1].count, 1);
}

// 14c. test_llm_metrics_by_model_and_purpose
#[test]
fn test_llm_metrics_by_model_and_purpose() {
    let conn = setup();

    // gpt-4o for conversation
    let m1 = LlmMetricsRow {
        id: "mp-1".to_string(),
        agent_id: "agent-1".to_string(),
        session_id: Some("s-1".to_string()),
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        purpose: "conversation".to_string(),
        task_type: Some("chat".to_string()),
        complexity: None,
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        estimated_cost_usd: 0.005,
        latency_ms: 2000,
        time_to_first_token_ms: None,
    };
    // gpt-4o for analysis
    let m2 = LlmMetricsRow {
        id: "mp-2".to_string(),
        purpose: "analysis".to_string(),
        estimated_cost_usd: 0.008,
        latency_ms: 3000,
        ..m1.clone()
    };
    // gpt-4o-mini for conversation
    let m3 = LlmMetricsRow {
        id: "mp-3".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "conversation".to_string(),
        estimated_cost_usd: 0.001,
        latency_ms: 400,
        ..m1.clone()
    };
    // gpt-4o-mini for analysis
    let m4 = LlmMetricsRow {
        id: "mp-4".to_string(),
        model: "gpt-4o-mini".to_string(),
        purpose: "analysis".to_string(),
        estimated_cost_usd: 0.0015,
        latency_ms: 500,
        ..m1.clone()
    };

    insert_llm_metrics(&conn, &m1).unwrap();
    insert_llm_metrics(&conn, &m2).unwrap();
    insert_llm_metrics(&conn, &m3).unwrap();
    insert_llm_metrics(&conn, &m4).unwrap();

    let stats = get_llm_metrics_by_model_and_purpose(&conn, "agent-1", "2020-01-01").unwrap();
    // Should have 4 entries: (gpt-4o, analysis), (gpt-4o, conversation), (gpt-4o-mini, analysis), (gpt-4o-mini, conversation)
    assert_eq!(stats.len(), 4);

    // Verify each entry has correct purpose.
    let purposes: Vec<&str> = stats.iter().map(|s| s.purpose.as_str()).collect();
    assert!(purposes.contains(&"conversation"));
    assert!(purposes.contains(&"analysis"));

    // Verify we can distinguish same model in different purposes.
    let gpt4o_conv = stats
        .iter()
        .find(|s| s.model == "gpt-4o" && s.purpose == "conversation")
        .unwrap();
    let gpt4o_anl = stats
        .iter()
        .find(|s| s.model == "gpt-4o" && s.purpose == "analysis")
        .unwrap();
    assert!((gpt4o_conv.total_cost - 0.005).abs() < 1e-9);
    assert!((gpt4o_anl.total_cost - 0.008).abs() < 1e-9);
}
