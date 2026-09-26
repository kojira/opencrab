#[test]
fn s10_fresh_schema_omits_legacy_gateway_tables_but_retains_generic_state() {
    let conn = crate::init_memory().expect("fresh core schema");
    for table in [
        "trusted_users",
        "channel_config",
        "session_watches",
        "agent_discord_config",
        "agent_nostr_config",
    ] {
        assert!(
            !table_exists(&conn, table).expect("inspect fresh schema"),
            "fresh core schema must not create legacy gateway table {table}"
        );
    }
    for table in ["api_principals", "agents", "sessions", "gate_instances", "deliveries"] {
        assert!(
            table_exists(&conn, table).expect("inspect retained core schema"),
            "fresh core schema must retain generic table {table}"
        );
    }
}
