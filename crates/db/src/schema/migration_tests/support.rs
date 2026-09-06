use rusqlite::Connection;

/// v20 相当（旧一意制約）の `impressions` を作り直し、行を入れて version 20 へ戻す。
pub(super) fn seed_legacy_impressions(conn: &Connection, rows: &str) {
    conn.execute_batch(&format!(
        "DROP TABLE impressions;
             CREATE TABLE impressions (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                target_name TEXT NOT NULL,
                personality TEXT DEFAULT '',
                communication_style TEXT DEFAULT '',
                recent_behavior TEXT DEFAULT '',
                agreement TEXT DEFAULT '中立',
                notes TEXT DEFAULT '',
                last_updated_turn INTEGER DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE(agent_id, session_id, target_id)
             );
             CREATE INDEX idx_impressions_session ON impressions(agent_id, session_id);
             INSERT INTO impressions
               (id, agent_id, session_id, target_id, target_name, personality,
                communication_style, recent_behavior, agreement, notes,
                last_updated_turn, created_at, updated_at)
               VALUES {rows};
             PRAGMA user_version = 20"
    ))
    .unwrap();
}

pub(super) fn user_tables(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT name FROM sqlite_master
         WHERE type='table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )
    .unwrap()
    .query_map([], |r| r.get(0))
    .unwrap()
    .collect::<Result<Vec<String>, _>>()
    .unwrap()
}
