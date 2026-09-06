use super::*;

// ============================================
// Agent Webhook Config tests
// ============================================

fn sample_webhook_row(agent_id: &str) -> AgentWebhookConfigRow {
    AgentWebhookConfigRow {
        scope: "agent".into(),
        agent_id: agent_id.into(),
        tool_name: "".into(),
        kind: "subtask".into(),
        url: "https://example.com/hook".into(),
        events_json: Some(r#"["start","done"]"#.into()),
        enabled: true,
        name: Some("default hook".into()),
        created_by: Some("tester".into()),
        output_mode: "full".into(),
        max_chars: 2000,
        updated_at: String::new(),
    }
}

#[test]
fn test_agent_webhook_upsert_and_get_roundtrip() {
    let conn = setup();
    let row = sample_webhook_row("agent-1");
    upsert_agent_webhook_config(&conn, &row).unwrap();

    let fetched = get_agent_webhook_config(&conn, "agent", "agent-1", "", "subtask")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.scope, "agent");
    assert_eq!(fetched.agent_id, "agent-1");
    assert_eq!(fetched.tool_name, "");
    assert_eq!(fetched.kind, "subtask");
    assert_eq!(fetched.url, "https://example.com/hook");
    assert_eq!(fetched.events_json, Some(r#"["start","done"]"#.to_string()));
    assert!(fetched.enabled);
    assert_eq!(fetched.name, Some("default hook".to_string()));
    assert_eq!(fetched.created_by, Some("tester".to_string()));
    assert_eq!(fetched.output_mode, "full");
    assert_eq!(fetched.max_chars, 2000);
    assert!(!fetched.updated_at.is_empty());
}

#[test]
fn test_agent_webhook_get_missing_returns_none() {
    let conn = setup();
    let result = get_agent_webhook_config(&conn, "agent", "nope", "", "subtask").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_agent_webhook_upsert_updates_not_duplicates() {
    let conn = setup();
    let mut row = sample_webhook_row("agent-1");
    upsert_agent_webhook_config(&conn, &row).unwrap();

    row.url = "https://example.com/updated".into();
    upsert_agent_webhook_config(&conn, &row).unwrap();

    let fetched = get_agent_webhook_config(&conn, "agent", "agent-1", "", "subtask")
        .unwrap()
        .unwrap();
    assert_eq!(fetched.url, "https://example.com/updated");

    // Only one row for this PK
    let all = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    let count = all
        .iter()
        .filter(|r| {
            r.scope == "agent"
                && r.agent_id == "agent-1"
                && r.tool_name.is_empty()
                && r.kind == "subtask"
        })
        .count();
    assert_eq!(count, 1);
}

#[test]
fn test_agent_webhook_list_include_disabled_filter() {
    let conn = setup();
    let mut enabled_row = sample_webhook_row("agent-1");
    enabled_row.kind = "subtask".into();
    upsert_agent_webhook_config(&conn, &enabled_row).unwrap();

    let mut disabled_row = sample_webhook_row("agent-1");
    disabled_row.kind = "tool".into();
    disabled_row.enabled = false;
    upsert_agent_webhook_config(&conn, &disabled_row).unwrap();

    let only_enabled = list_agent_webhook_config(&conn, Some("agent-1"), false).unwrap();
    assert_eq!(only_enabled.len(), 1);
    assert_eq!(only_enabled[0].kind, "subtask");

    let with_disabled = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    assert_eq!(with_disabled.len(), 2);
}

#[test]
fn test_agent_webhook_list_agent_includes_global() {
    let conn = setup();
    upsert_agent_webhook_config(&conn, &sample_webhook_row("agent-1")).unwrap();

    let mut global = sample_webhook_row("*");
    global.scope = "global".into();
    upsert_agent_webhook_config(&conn, &global).unwrap();

    upsert_agent_webhook_config(&conn, &sample_webhook_row("agent-2")).unwrap();

    let rows = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    let agent_ids: Vec<&str> = rows.iter().map(|r| r.agent_id.as_str()).collect();
    assert!(agent_ids.contains(&"agent-1"));
    assert!(agent_ids.contains(&"*"));
    assert!(!agent_ids.contains(&"agent-2"));
    assert_eq!(rows.len(), 2);

    // None -> all rows
    let all = list_agent_webhook_config(&conn, None, true).unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn test_agent_webhook_distinct_pk_combos_coexist() {
    let conn = setup();

    let mut r1 = sample_webhook_row("agent-1");
    r1.kind = "subtask".into();
    let mut r2 = sample_webhook_row("agent-1");
    r2.kind = "tool".into();
    r2.tool_name = "my_tool".into();
    let mut r3 = sample_webhook_row("agent-1");
    r3.scope = "tool".into();
    r3.kind = "lifecycle".into();

    upsert_agent_webhook_config(&conn, &r1).unwrap();
    upsert_agent_webhook_config(&conn, &r2).unwrap();
    upsert_agent_webhook_config(&conn, &r3).unwrap();

    let rows = list_agent_webhook_config(&conn, Some("agent-1"), true).unwrap();
    assert_eq!(rows.len(), 3);

    assert!(
        get_agent_webhook_config(&conn, "agent", "agent-1", "", "subtask")
            .unwrap()
            .is_some()
    );
    assert!(
        get_agent_webhook_config(&conn, "agent", "agent-1", "my_tool", "tool")
            .unwrap()
            .is_some()
    );
    assert!(
        get_agent_webhook_config(&conn, "tool", "agent-1", "", "lifecycle")
            .unwrap()
            .is_some()
    );
}
