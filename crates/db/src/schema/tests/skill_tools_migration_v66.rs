/// D-1143 v66 on a v65 database with skills, including one marked `agent_visible` whose
/// `situation_pattern` names `execute_shell`: the column is added and every existing row gets
/// `[]` (nothing is copied, so no existing skill grants a tool); re-running is a no-op.
#[test]
fn v66_adds_empty_skill_tools_without_backfill() {
    let conn = Connection::open_in_memory().unwrap();
    initialize_historical(&conn).expect("historical schema to 62");
    run_migrations(&conn, MIGRATIONS.into_iter().filter(|m| m.version <= 65)).expect("to 65");
    assert_eq!(schema_version(&conn).unwrap(), 65);
    // The baseline here runs today's `SCHEMA_SQL`, which already has the column; a production
    // database at v65 does not.
    conn.execute_batch("ALTER TABLE skills DROP COLUMN tools").unwrap();
    conn.execute(
        "INSERT INTO skills (id, agent_id, name, description, situation_pattern, guidance,
             agent_visible, created_at, updated_at)
         VALUES ('s1', 'a', 'local', 'd', '[\"execute_shell\"]', 'g', 1, 'now', 'now')",
        [],
    )
    .unwrap();

    initialize(&conn).expect("upgrade 65 -> 66");
    initialize(&conn).expect("re-run is a no-op");

    assert_eq!(schema_version(&conn).unwrap(), 66);
    let tools: String = conn
        .query_row("SELECT tools FROM skills WHERE id = 's1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tools, "[]");
}

/// A fresh database has `skills.tools`.
#[test]
fn v66_fresh_database_has_skill_tools() {
    let conn = crate::init_memory().expect("fresh");
    assert_eq!(schema_version(&conn).unwrap(), 66);
    assert!(column_exists(&conn, "skills", "tools").unwrap());
}
