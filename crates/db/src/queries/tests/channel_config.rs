use super::*;

// ── Discord Channel Config ──

#[test]
fn test_channel_config_upsert_and_get() {
    let conn = setup();

    let cfg = ChannelConfigRow {
        channel_id: "123456".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "general".to_string(),
        readable: true,
        writable: false,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };

    upsert_channel_config(&conn, &cfg).unwrap();

    let fetched = get_channel_config(&conn, "123456").unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.channel_id, "123456");
    assert_eq!(fetched.guild_id, "guild-1");
    assert_eq!(fetched.channel_name, "general");
    assert!(fetched.readable);
    assert!(!fetched.writable);
}

#[test]
fn test_channel_config_upsert_update() {
    let conn = setup();

    let cfg = ChannelConfigRow {
        channel_id: "123456".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "general".to_string(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    upsert_channel_config(&conn, &cfg).unwrap();

    // Update writable to false
    let cfg2 = ChannelConfigRow {
        writable: false,
        ..cfg
    };
    upsert_channel_config(&conn, &cfg2).unwrap();

    let fetched = get_channel_config(&conn, "123456").unwrap().unwrap();
    assert!(fetched.readable);
    assert!(!fetched.writable);
}

#[test]
fn test_channel_config_list_by_guild() {
    let conn = setup();

    let cfg1 = ChannelConfigRow {
        channel_id: "ch-1".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "general".to_string(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    let cfg2 = ChannelConfigRow {
        channel_id: "ch-2".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "random".to_string(),
        readable: false,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    let cfg3 = ChannelConfigRow {
        channel_id: "ch-3".to_string(),
        agent_id: String::new(),
        guild_id: "guild-2".to_string(),
        channel_name: "other".to_string(),
        readable: true,
        writable: true,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };

    upsert_channel_config(&conn, &cfg1).unwrap();
    upsert_channel_config(&conn, &cfg2).unwrap();
    upsert_channel_config(&conn, &cfg3).unwrap();

    let results = list_channel_configs_by_guild(&conn, "guild-1").unwrap();
    assert_eq!(results.len(), 2);

    let results2 = list_channel_configs_by_guild(&conn, "guild-2").unwrap();
    assert_eq!(results2.len(), 1);
}

#[test]
fn test_is_channel_readable_writable_defaults() {
    let conn = setup();

    // No config → defaults to true
    assert!(is_channel_readable(&conn, "unknown-ch"));
    assert!(is_channel_writable(&conn, "unknown-ch"));

    // Set readable=false
    let cfg = ChannelConfigRow {
        channel_id: "ch-blocked".to_string(),
        agent_id: String::new(),
        guild_id: "guild-1".to_string(),
        channel_name: "blocked".to_string(),
        readable: false,
        writable: false,
        whitelisted: false,
        heartbeat_enabled: true,
        heartbeat_interval_secs: None,
        heartbeat_instructions: String::new(),
    };
    upsert_channel_config(&conn, &cfg).unwrap();

    assert!(!is_channel_readable(&conn, "ch-blocked"));
    assert!(!is_channel_writable(&conn, "ch-blocked"));
}
