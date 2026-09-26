// Historical v56 source tables are absent from a newly initialized S10 core.
// Migration fixtures explicitly model the stopped pre-cleanup source instead.
fn create_legacy_core_source_tables(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "CREATE TABLE channel_config (
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
            PRIMARY KEY(channel_id,agent_id)
         );
         CREATE TABLE trusted_users (
            id TEXT PRIMARY KEY,
            user_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            permission TEXT NOT NULL DEFAULT 'user',
            created_by TEXT NOT NULL DEFAULT 'owner',
            created_at TEXT NOT NULL,
            display_name TEXT NOT NULL DEFAULT '',
            platform TEXT NOT NULL DEFAULT 'external',
            UNIQUE(user_id,agent_id)
         );
         CREATE TABLE session_watches (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            interval_secs INTEGER NOT NULL,
            filter_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            CHECK(interval_secs > 0)
         );",
    )
    .unwrap();
}
