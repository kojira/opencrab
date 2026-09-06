use super::*;

// ── Agent Discord Config ──

#[test]
fn test_agent_discord_config_upsert_and_get() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-1".to_string(),
        bot_token: "TOKEN_ABC_12345".to_string(),
        owner_discord_id: "390123456789".to_string(),
        enabled: true,
    };

    upsert_agent_discord_config(&conn, &cfg).unwrap();

    let fetched = get_agent_discord_config(&conn, "agent-1").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.agent_id, "agent-1");
    assert_eq!(fetched.bot_token, "TOKEN_ABC_12345");
    assert_eq!(fetched.owner_discord_id, "390123456789");
    assert!(fetched.enabled);
}

#[test]
fn test_agent_discord_config_get_nonexistent() {
    let conn = setup();
    let result = get_agent_discord_config(&conn, "no-such-agent").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_agent_discord_config_upsert_update() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-1".to_string(),
        bot_token: "OLD_TOKEN".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    upsert_agent_discord_config(&conn, &cfg).unwrap();

    // Update token and owner
    let cfg2 = AgentDiscordConfigRow {
        agent_id: "agent-1".to_string(),
        bot_token: "NEW_TOKEN".to_string(),
        owner_discord_id: "999888777".to_string(),
        enabled: false,
    };
    upsert_agent_discord_config(&conn, &cfg2).unwrap();

    let fetched = get_agent_discord_config(&conn, "agent-1").unwrap().unwrap();
    assert_eq!(fetched.bot_token, "NEW_TOKEN");
    assert_eq!(fetched.owner_discord_id, "999888777");
    assert!(!fetched.enabled);
}

#[test]
fn test_agent_discord_config_delete() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-del".to_string(),
        bot_token: "TOKEN".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    upsert_agent_discord_config(&conn, &cfg).unwrap();
    assert!(get_agent_discord_config(&conn, "agent-del")
        .unwrap()
        .is_some());

    let deleted = delete_agent_discord_config(&conn, "agent-del").unwrap();
    assert!(deleted);
    assert!(get_agent_discord_config(&conn, "agent-del")
        .unwrap()
        .is_none());

    // Delete nonexistent → false
    let deleted2 = delete_agent_discord_config(&conn, "agent-del").unwrap();
    assert!(!deleted2);
}

#[test]
fn test_list_enabled_agent_discord_configs() {
    let conn = setup();

    let cfg1 = AgentDiscordConfigRow {
        agent_id: "a1".to_string(),
        bot_token: "T1".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    let cfg2 = AgentDiscordConfigRow {
        agent_id: "a2".to_string(),
        bot_token: "T2".to_string(),
        owner_discord_id: "".to_string(),
        enabled: false, // disabled
    };
    let cfg3 = AgentDiscordConfigRow {
        agent_id: "a3".to_string(),
        bot_token: "T3".to_string(),
        owner_discord_id: "owner".to_string(),
        enabled: true,
    };

    upsert_agent_discord_config(&conn, &cfg1).unwrap();
    upsert_agent_discord_config(&conn, &cfg2).unwrap();
    upsert_agent_discord_config(&conn, &cfg3).unwrap();

    let enabled = list_enabled_agent_discord_configs(&conn).unwrap();
    assert_eq!(enabled.len(), 2);

    let ids: Vec<&str> = enabled.iter().map(|c| c.agent_id.as_str()).collect();
    assert!(ids.contains(&"a1"));
    assert!(ids.contains(&"a3"));
    assert!(!ids.contains(&"a2"));
}

#[test]
fn test_set_agent_discord_config_enabled() {
    let conn = setup();

    let cfg = AgentDiscordConfigRow {
        agent_id: "agent-toggle".to_string(),
        bot_token: "TOKEN".to_string(),
        owner_discord_id: "".to_string(),
        enabled: true,
    };
    upsert_agent_discord_config(&conn, &cfg).unwrap();

    // Initially enabled
    let fetched = get_agent_discord_config(&conn, "agent-toggle")
        .unwrap()
        .unwrap();
    assert!(fetched.enabled);

    // Disable
    let updated = set_agent_discord_config_enabled(&conn, "agent-toggle", false).unwrap();
    assert!(updated);
    let fetched = get_agent_discord_config(&conn, "agent-toggle")
        .unwrap()
        .unwrap();
    assert!(!fetched.enabled);

    // Re-enable
    let updated = set_agent_discord_config_enabled(&conn, "agent-toggle", true).unwrap();
    assert!(updated);
    let fetched = get_agent_discord_config(&conn, "agent-toggle")
        .unwrap()
        .unwrap();
    assert!(fetched.enabled);

    // Nonexistent agent → false
    let updated = set_agent_discord_config_enabled(&conn, "no-such", false).unwrap();
    assert!(!updated);
}

#[test]
fn test_delete_agent_also_removes_discord_config() {
    let conn = setup();

    let agent_id = "agent-discord-del";
    upsert_agent(
        &conn,
        &AgentRow {
            agent_id: agent_id.into(),
            name: "DiscordAgent".into(),
            job_title: None,
            organization: None,
            image_url: None,
            persona_name: "d".into(),
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
    upsert_agent_discord_config(
        &conn,
        &AgentDiscordConfigRow {
            agent_id: agent_id.into(),
            bot_token: "BOT_TOKEN_123".into(),
            owner_discord_id: "owner-1".into(),
            enabled: true,
        },
    )
    .unwrap();

    // Verify exists
    assert!(get_agent_discord_config(&conn, agent_id).unwrap().is_some());

    // Delete agent
    let deleted = delete_agent(&conn, agent_id).unwrap();
    assert!(deleted);

    // Discord config should also be gone
    assert!(get_agent_discord_config(&conn, agent_id).unwrap().is_none());
}
