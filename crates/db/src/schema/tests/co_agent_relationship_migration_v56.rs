#[test]
fn s6_v56_versions_internal_co_agent_relationships_on_fresh_and_upgrade() {
    let fresh = Connection::open_in_memory().unwrap();
    initialize(&fresh).unwrap();
    assert!(column_exists(&fresh, "trusted_co_agents", "relationship_revision").unwrap());
    assert!(column_exists(&fresh, "trusted_co_agents", "active").unwrap());

    let upgraded = Connection::open_in_memory().unwrap();
    initialize(&upgraded).unwrap();
    upgraded
        .execute_batch(
            "ALTER TABLE trusted_co_agents RENAME TO trusted_co_agents_new;
             CREATE TABLE trusted_co_agents (
               id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, co_agent_id TEXT NOT NULL,
               allowed_actions TEXT, created_by TEXT NOT NULL, created_at DATETIME NOT NULL,
               UNIQUE(agent_id, co_agent_id)
             );
             INSERT INTO trusted_co_agents(id,agent_id,co_agent_id,allowed_actions,created_by,created_at)
             SELECT id,agent_id,co_agent_id,allowed_actions,created_by,created_at FROM trusted_co_agents_new;
             DROP TABLE trusted_co_agents_new;
             PRAGMA user_version=55;",
        )
        .unwrap();
    initialize(&upgraded).unwrap();
    assert_eq!(schema_version(&upgraded).unwrap(), 56);
    assert!(column_exists(&upgraded, "trusted_co_agents", "relationship_revision").unwrap());
    assert!(column_exists(&upgraded, "trusted_co_agents", "active").unwrap());
}
