#[test]
fn v50_migrates_channel_config_without_losing_values_or_leaving_two_authorities() {
    use crate::queries::{get_channel_config, get_channel_config_for_agent};
    use rusqlite::params;

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "opencrab-v50-channel-config-{}-{unique}.sqlite",
        std::process::id()
    ));
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE discord_channel_config (
                channel_id TEXT NOT NULL,
                agent_id TEXT NOT NULL DEFAULT '',
                guild_id TEXT NOT NULL,
                channel_name TEXT NOT NULL DEFAULT '',
                readable INTEGER NOT NULL DEFAULT 1,
                writable INTEGER NOT NULL DEFAULT 1,
                whitelisted INTEGER NOT NULL DEFAULT 0,
                heartbeat_enabled INTEGER NOT NULL DEFAULT 1,
                heartbeat_interval_secs INTEGER,
                heartbeat_instructions TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL,
                PRIMARY KEY (channel_id, agent_id)
             );
             CREATE INDEX idx_discord_channel_guild
               ON discord_channel_config(guild_id);
             CREATE TABLE gate_bindings (
                binding_id TEXT,
                instance_id TEXT,
                address TEXT,
                closed_at INTEGER
             );
             PRAGMA user_version = 49;",
        )
        .unwrap();
        let rows = [
            (
                "opaque-channel",
                "",
                "opaque-guild",
                "general",
                0_i64,
                1_i64,
                1_i64,
                0_i64,
                None,
                "",
                "2026-01-02T03:04:05.006Z",
            ),
            (
                "opaque-channel",
                "agent-α",
                "guild-\0-value",
                "名前\n二行目",
                1,
                0,
                0,
                1,
                Some(i64::MAX),
                "instruction\0bytes\n続き",
                "not-normalized-but-preserved",
            ),
            (
                "edge-channel",
                "agent-zero",
                "",
                "",
                0,
                0,
                0,
                0,
                Some(0),
                " ",
                "",
            ),
        ];
        for row in rows {
            conn.execute(
                "INSERT INTO discord_channel_config VALUES
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9,
                    row.10
                ],
            )
            .unwrap();
        }

        initialize(&conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 51);
        assert!(table_exists(&conn, "channel_config").unwrap());
        assert!(!table_exists(&conn, "discord_channel_config").unwrap());

        let global = get_channel_config(&conn, "opaque-channel").unwrap().unwrap();
        assert_eq!(global.agent_id, "");
        assert_eq!(global.guild_id, "opaque-guild");
        assert_eq!(global.channel_name, "general");
        assert!(!global.readable);
        assert!(global.writable);
        assert!(global.whitelisted);
        assert!(!global.heartbeat_enabled);
        assert_eq!(global.heartbeat_interval_secs, None);
        assert_eq!(global.heartbeat_instructions, "");

        let scoped = get_channel_config_for_agent(&conn, "opaque-channel", "agent-α")
            .unwrap()
            .unwrap();
        assert_eq!(scoped.guild_id.as_bytes(), b"guild-\0-value");
        assert_eq!(scoped.channel_name, "名前\n二行目");
        assert!(scoped.readable);
        assert!(!scoped.writable);
        assert!(!scoped.whitelisted);
        assert!(scoped.heartbeat_enabled);
        assert_eq!(scoped.heartbeat_interval_secs, Some(i64::MAX as u64));
        assert_eq!(scoped.heartbeat_instructions, "instruction\0bytes\n続き");

        let edge = get_channel_config_for_agent(&conn, "edge-channel", "agent-zero")
            .unwrap()
            .unwrap();
        assert_eq!(edge.guild_id, "");
        assert_eq!(edge.channel_name, "");
        assert_eq!(edge.heartbeat_interval_secs, Some(0));
        assert_eq!(edge.heartbeat_instructions, " ");

        let updated: String = conn
            .query_row(
                "SELECT updated_at FROM channel_config
                 WHERE channel_id = 'opaque-channel' AND agent_id = 'agent-α'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(updated, "not-normalized-but-preserved");
        for index in ["idx_channel_guild", "idx_channel_agent"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'index' AND name = ?1 AND tbl_name = 'channel_config'",
                    [index],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing {index}");
        }
        initialize(&conn).unwrap();
    }
    {
        let conn = Connection::open(&path).unwrap();
        initialize(&conn).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 51);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM channel_config", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert!(!table_exists(&conn, "discord_channel_config").unwrap());
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn v50_preserves_heartbeat_audit_values_with_opaque_caller_id() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE heartbeat_instructions_audit (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            scope TEXT NOT NULL,
            channel_id TEXT,
            caller_identity TEXT NOT NULL,
            legacy_caller_id TEXT,
            old_value TEXT,
            new_value TEXT,
            reason TEXT,
            created_at TEXT NOT NULL
         );
         INSERT INTO heartbeat_instructions_audit
           (agent_id, scope, channel_id, caller_identity, legacy_caller_id,
            old_value, new_value, reason, created_at)
         VALUES ('a1', 'channel', 'c1', 'owner', 'u1', 'old', 'new', 'test', 'now');
         CREATE TABLE gate_bindings (
            binding_id TEXT,
            instance_id TEXT,
            address TEXT,
            closed_at INTEGER
         );
         PRAGMA user_version = 49;",
    )
    .unwrap();

    run_migrations(&conn, MIGRATION_GROUPS.iter().flat_map(|group| group.iter())).unwrap();

    assert!(column_exists(&conn, "heartbeat_instructions_audit", "caller_user_id").unwrap());
    let row: (String, String, String) = conn
        .query_row(
            "SELECT agent_id, caller_user_id, new_value FROM heartbeat_instructions_audit",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, ("a1".into(), "u1".into(), "new".into()));
}
