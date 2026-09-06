use super::*;

#[test]
fn test_agent_upsert_and_get() {
    let conn = setup();
    let agent = AgentRow {
        agent_id: "agent-1".to_string(),
        name: "Alice".to_string(),
        job_title: Some("Engineer".to_string()),
        organization: Some("OpenCrab Inc.".to_string()),
        image_url: Some("https://example.com/avatar.png".to_string()),
        persona_name: "Crab".to_string(),
        personality: Some(r#"{"hobby":"coding"}"#.to_string()),
        instructions: String::new(),
        heartbeat_instructions: String::new(),
        model: None,
        reasoning_effort: None,
        web_search: None,
        metadata_json: Some(r#"{"lang":"en"}"#.to_string()),
    };

    upsert_agent(&conn, &agent).unwrap();

    let fetched = get_agent(&conn, "agent-1").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.agent_id, "agent-1");
    assert_eq!(fetched.name, "Alice");
    assert_eq!(fetched.persona_name, "Crab");
    assert_eq!(
        fetched.personality,
        Some(r#"{"hobby":"coding"}"#.to_string())
    );
    assert_eq!(fetched.job_title, Some("Engineer".to_string()));
    assert_eq!(
        fetched.image_url,
        Some("https://example.com/avatar.png".to_string())
    );
    assert_eq!(fetched.metadata_json, Some(r#"{"lang":"en"}"#.to_string()));
}

#[test]
fn test_agent_get_nonexistent() {
    let conn = setup();
    let result = get_agent(&conn, "nonexistent-agent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_effective_model_for_agent() {
    let conn = setup();
    let agent = AgentRow {
        agent_id: "a1".to_string(),
        name: "N".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        persona_name: "p".to_string(),
        personality: None,
        instructions: String::new(),
        heartbeat_instructions: String::new(),
        model: Some("openai:gpt-4o".to_string()),
        reasoning_effort: None,
        web_search: None,
        metadata_json: None,
    };
    upsert_agent(&conn, &agent).unwrap();
    let m = effective_model_for_agent(&conn, "a1", "anthropic:claude").unwrap();
    assert_eq!(m, "openai:gpt-4o");
    let m2 = effective_model_for_agent(&conn, "a1", "anthropic:claude").unwrap();
    assert_eq!(m2, "openai:gpt-4o");

    let agent2 = AgentRow {
        agent_id: "a2".to_string(),
        name: "N2".to_string(),
        job_title: None,
        organization: None,
        image_url: None,
        persona_name: "p".to_string(),
        personality: None,
        instructions: String::new(),
        heartbeat_instructions: String::new(),
        model: None,
        reasoning_effort: None,
        web_search: None,
        metadata_json: None,
    };
    upsert_agent(&conn, &agent2).unwrap();
    let m3 = effective_model_for_agent(&conn, "a2", "global:default").unwrap();
    assert_eq!(m3, "global:default");
}

// ── delete_agent ──

#[test]
fn test_delete_agent() {
    let conn = setup();

    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "del-1".into(),
            name: "DeleteMe".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "Doomed".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    upsert_curated_memory(
        &conn,
        &CuratedMemoryRow {
            id: "cm-del-1".into(),
            agent_id: "del-1".into(),
            category: "fact".into(),
            content: "will be deleted".into(),
            created_at: String::new(),
        },
    )
    .unwrap();

    assert!(get_agent(&conn, "del-1").unwrap().is_some());

    let deleted = delete_agent(&conn, "del-1").unwrap();
    assert!(deleted);

    assert!(get_agent(&conn, "del-1").unwrap().is_none());
    assert!(list_curated_memories(&conn, "del-1", 10000, 0)
        .unwrap()
        .0
        .is_empty());
}

#[test]
fn test_delete_agent_nonexistent() {
    let conn = setup();
    let deleted = delete_agent(&conn, "no-such-agent").unwrap();
    assert!(!deleted);
}

// ── find_agents ──

#[test]
fn test_find_agents_by_id_prefix() {
    let conn = setup();
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "abc-12345".into(),
            name: "Alice".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "a".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "xyz-99999".into(),
            name: "Bob".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "b".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    // Search by ID prefix
    let results = find_agents(&conn, "abc").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "Alice");

    // Search by name
    let results = find_agents(&conn, "bob").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "Bob");

    // No match
    let results = find_agents(&conn, "zzz").unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_find_agents_partial_name() {
    let conn = setup();
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: "agent-find-1".into(),
            name: "Creative Researcher".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "cr".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    let results = find_agents(&conn, "creative").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "Creative Researcher");

    let results = find_agents(&conn, "researcher").unwrap();
    assert_eq!(results.len(), 1);
}

// ── Agent CRUD full cycle ──

#[test]
fn test_agent_crud_full_cycle() {
    let conn = setup();

    let agent_id = "crud-agent-1";
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: agent_id.into(),
            name: "TestAgent".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "Original Persona".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    let row = get_agent(&conn, agent_id).unwrap().unwrap();
    assert_eq!(row.name, "TestAgent");
    assert_eq!(row.persona_name, "Original Persona");

    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: agent_id.into(),
            name: "UpdatedAgent".into(),
            job_title: Some("Lead".into()),
            organization: None,
            image_url: None,
            persona_name: "Updated Persona".into(),
            personality: None,
            instructions: String::new(),
            heartbeat_instructions: String::new(),
            model: None,
            reasoning_effort: None,
            web_search: None,
            metadata_json: None,
        },
    )
    .unwrap();

    let row = get_agent(&conn, agent_id).unwrap().unwrap();
    assert_eq!(row.name, "UpdatedAgent");
    assert_eq!(row.job_title, Some("Lead".to_string()));
    assert_eq!(row.persona_name, "Updated Persona");

    // Find
    let results = find_agents(&conn, "Updated").unwrap();
    assert_eq!(results.len(), 1);

    // Delete
    let deleted = delete_agent(&conn, agent_id).unwrap();
    assert!(deleted);
    assert!(get_agent(&conn, agent_id).unwrap().is_none());

    // Find after delete
    let results = find_agents(&conn, "Updated").unwrap();
    assert!(results.is_empty());
}
