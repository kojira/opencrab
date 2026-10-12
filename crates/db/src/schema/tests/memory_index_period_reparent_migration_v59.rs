// D-1078: v59 moves topics that sit under the month their long-lived session started in
// to the month they happened in (topic.date_from), and marks every touched month stale.

/// One `memory_index_nodes` row for the v59 fixture.
#[allow(clippy::too_many_arguments)]
fn v59_node(
    conn: &Connection,
    id: &str,
    parent: Option<&str>,
    node_type: &str,
    title: &str,
    source_session_id: Option<&str>,
    date_from: Option<&str>,
    short_id: &str,
    refreshed: Option<&str>,
) {
    conn.execute(
        "INSERT INTO memory_index_nodes
           (id, agent_id, parent_id, node_type, source_type, title, summary, start_log_id,
            end_log_id, source_session_id, date_from, date_to, depth, child_count, token_count,
            created_at, updated_at, short_id, keywords_json, summary_refreshed_at)
         VALUES (?1, 'a1', ?2, ?3, 'session_log', ?4, ?4 || ' summary', 1, 2, ?5, ?6, ?6, 0, 0, 0,
                 '2025-08-01T00:00:00Z', '2025-08-01T00:00:00Z', ?7, '[]', ?8)",
        rusqlite::params![id, parent, node_type, title, source_session_id, date_from, short_id, refreshed],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memory_index_fts (title, summary, keywords, node_id, agent_id, node_type, source_type)
         VALUES (?1, ?1 || ' summary', '', ?2, 'a1', ?3, 'session_log')",
        rusqlite::params![title, id, node_type],
    )
    .unwrap();
}

/// The bad pre-v59 state: session `nostr` started in 2025-08 and all its later topics
/// (2025-09, 2025-10) are still under 2025-08. Session `late` sits under 2025-08 but has
/// only a 2025-09 topic. 2025-09 already exists (rolled); 2025-10 does not exist yet.
fn v59_seed_misparented_index(conn: &Connection) {
    let rolled = Some("2025-11-01T00:00:00Z");
    v59_node(conn, "root-a1", None, "root", "Memory Root", None, None, "r0", None);
    v59_node(conn, "period-a1-2025-08", Some("root-a1"), "period", "2025-08", None, None, "p1", rolled);
    v59_node(conn, "period-a1-2025-09", Some("root-a1"), "period", "2025-09", None, None, "p2", rolled);
    v59_node(conn, "session-a1-nostr", Some("period-a1-2025-08"), "session", "Session: nostr", Some("nostr"), None, "s1", None);
    v59_node(conn, "session-a1-other", Some("period-a1-2025-09"), "session", "Session: other", Some("other"), None, "s2", None);
    v59_node(conn, "session-a1-late", Some("period-a1-2025-08"), "session", "Session: late", Some("late"), None, "s3", None);
    let s = "session-a1-nostr";
    v59_node(conn, "topic-aug", Some(s), "topic", "aug", Some("nostr"), Some("2025-08-05"), "t1", None);
    v59_node(conn, "topic-sep-1", Some(s), "topic", "sep1", Some("nostr"), Some("2025-09-10"), "t2", None);
    v59_node(conn, "topic-sep-2", Some(s), "topic", "sep2", Some("nostr"), Some("2025-09-20"), "t3", None);
    v59_node(conn, "topic-oct", Some(s), "topic", "oct", Some("nostr"), Some("2025-10-02"), "t4", None);
    v59_node(conn, "topic-undated", Some(s), "topic", "undated", Some("nostr"), None, "t5", None);
    v59_node(conn, "topic-other", Some("session-a1-other"), "topic", "other", Some("other"), Some("2025-09-01"), "t6", None);
    v59_node(conn, "topic-late", Some("session-a1-late"), "topic", "late", Some("late"), Some("2025-09-15"), "t7", None);
    conn.execute_batch("PRAGMA user_version = 58").unwrap();
}

fn v59_parent(conn: &Connection, id: &str) -> Option<String> {
    conn.query_row(
        "SELECT parent_id FROM memory_index_nodes WHERE id = ?1",
        [id],
        |r| r.get(0),
    )
    .unwrap()
}

/// (id, parent_id, short_id, summary_refreshed_at, child_count) of every node.
type V59NodeState = (String, Option<String>, Option<String>, Option<String>, i64);

fn v59_snapshot(conn: &Connection) -> Vec<V59NodeState> {
    conn.prepare(
        "SELECT id, parent_id, short_id, summary_refreshed_at, child_count
         FROM memory_index_nodes ORDER BY id",
    )
    .unwrap()
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

#[test]
fn v59_reparents_topics_to_their_own_month_and_marks_months_stale() {
    let conn = crate::init_memory().unwrap();
    v59_seed_misparented_index(&conn);
    let topics_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_index_nodes WHERE node_type = 'topic'", [], |r| r.get(0))
        .unwrap();

    initialize(&conn).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), 59);

    // Parents: every dated topic is under a session node whose period is its own month.
    assert_eq!(v59_parent(&conn, "topic-aug").as_deref(), Some("session-a1-nostr"));
    assert_eq!(v59_parent(&conn, "topic-undated").as_deref(), Some("session-a1-nostr"));
    assert_eq!(v59_parent(&conn, "topic-sep-1").as_deref(), Some("session-a1-nostr-2025-09"));
    assert_eq!(v59_parent(&conn, "topic-sep-2").as_deref(), Some("session-a1-nostr-2025-09"));
    assert_eq!(v59_parent(&conn, "topic-oct").as_deref(), Some("session-a1-nostr-2025-10"));
    assert_eq!(v59_parent(&conn, "topic-late").as_deref(), Some("session-a1-late-2025-09"));
    assert_eq!(v59_parent(&conn, "topic-other").as_deref(), Some("session-a1-other"));
    assert_eq!(v59_parent(&conn, "session-a1-nostr-2025-09").as_deref(), Some("period-a1-2025-09"));
    assert_eq!(v59_parent(&conn, "session-a1-nostr-2025-10").as_deref(), Some("period-a1-2025-10"));
    assert_eq!(v59_parent(&conn, "session-a1-late-2025-09").as_deref(), Some("period-a1-2025-09"));
    assert_eq!(v59_parent(&conn, "period-a1-2025-10").as_deref(), Some("root-a1"));
    let mismatched: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_index_nodes t
             JOIN memory_index_nodes s ON t.parent_id = s.id
             JOIN memory_index_nodes p ON s.parent_id = p.id
             WHERE t.node_type = 'topic' AND substr(t.date_from, 1, 7) != p.title",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mismatched, 0);
    // New session nodes keep the source session id (readers filter on it).
    let source: Option<String> = conn
        .query_row(
            "SELECT source_session_id FROM memory_index_nodes WHERE id = 'session-a1-nostr-2025-10'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(source.as_deref(), Some("nostr"));
    // The emptied legacy session node is removed (with its FTS row).
    let late_exists: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_index_nodes WHERE id = 'session-a1-late'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(late_exists, 0);

    // Stale: every touched month needs a refresh; the rollup query picks them up.
    for period in ["period-a1-2025-08", "period-a1-2025-09", "period-a1-2025-10"] {
        let refreshed: Option<String> = conn
            .query_row(
                "SELECT summary_refreshed_at FROM memory_index_nodes WHERE id = ?1",
                [period],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(refreshed, None, "{period} must be stale");
    }
    let stale = crate::queries::find_stale_period(&conn, "a1").unwrap().unwrap();
    assert_eq!(stale.title, "2025-08");

    // No topic lost.
    let topics_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_index_nodes WHERE node_type = 'topic'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(topics_after, topics_before);
    let counts = crate::queries::count_topics_per_period(&conn, "a1").unwrap();
    assert_eq!(counts.get("period-a1-2025-08"), Some(&2));
    assert_eq!(counts.get("period-a1-2025-09"), Some(&4));
    assert_eq!(counts.get("period-a1-2025-10"), Some(&1));

    // short_id stays unique and every new node has one.
    let (total, distinct, missing): (i64, i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COUNT(DISTINCT short_id), SUM(short_id IS NULL)
             FROM memory_index_nodes WHERE agent_id = 'a1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(missing, 0);
    assert_eq!(total, distinct);

    // FTS mirrors the node set (new nodes in, deleted nodes out).
    let fts_orphans: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_index_fts f
             WHERE NOT EXISTS (SELECT 1 FROM memory_index_nodes n WHERE n.id = f.node_id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_orphans, 0);
    let fts_missing: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_index_nodes n
             WHERE NOT EXISTS (SELECT 1 FROM memory_index_fts f WHERE f.node_id = n.id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_missing, 0);

    // child_count reflects the new tree.
    let child_count: i64 = conn
        .query_row(
            "SELECT child_count FROM memory_index_nodes WHERE id = 'session-a1-nostr-2025-09'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(child_count, 2);
}

#[test]
fn v59_is_idempotent() {
    let conn = crate::init_memory().unwrap();
    v59_seed_misparented_index(&conn);
    initialize(&conn).unwrap();
    let first = v59_snapshot(&conn);
    conn.execute_batch("PRAGMA user_version = 58").unwrap();
    initialize(&conn).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), 59);
    assert_eq!(v59_snapshot(&conn), first);
}

#[test]
fn v59_leaves_correctly_parented_index_untouched() {
    let conn = crate::init_memory().unwrap();
    let rolled = Some("2025-11-01T00:00:00Z");
    v59_node(&conn, "root-a1", None, "root", "Memory Root", None, None, "r0", None);
    v59_node(&conn, "period-a1-2025-08", Some("root-a1"), "period", "2025-08", None, None, "p1", rolled);
    v59_node(&conn, "session-a1-x", Some("period-a1-2025-08"), "session", "S", Some("x"), None, "s1", None);
    v59_node(&conn, "topic-x", Some("session-a1-x"), "topic", "x", Some("x"), Some("2025-08-03"), "t1", None);
    conn.execute_batch("PRAGMA user_version = 58").unwrap();
    let before = v59_snapshot(&conn);
    initialize(&conn).unwrap();
    assert_eq!(v59_snapshot(&conn), before, "a rolled month without moves stays rolled");
}


#[test]
fn v59_removed_session_node_leaves_no_category_member_rows() {
    let conn = crate::init_memory().unwrap();
    v59_seed_misparented_index(&conn);
    // `session-a1-late` loses its only topic and is removed by v59.
    conn.execute_batch(
        "INSERT INTO memory_category_members (agent_id, topic_id, category_id, created_at)
         VALUES ('a1', 'session-a1-late', 'cat-x', '2025-08-01T00:00:00Z'),
                ('a1', 'topic-other', 'session-a1-late', '2025-08-01T00:00:00Z');",
    )
    .unwrap();
    initialize(&conn).unwrap();
    let dangling: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_category_members
             WHERE topic_id = 'session-a1-late' OR category_id = 'session-a1-late'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dangling, 0);
}

fn v59_assert_child_count_matches(conn: &Connection, id: &str) {
    let (stored, actual): (i64, i64) = conn
        .query_row(
            "SELECT child_count,
                    (SELECT COUNT(*) FROM memory_index_nodes c WHERE c.parent_id = n.id)
             FROM memory_index_nodes n WHERE n.id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, actual, "{id} child_count");
}

#[test]
fn v59_recomputes_root_child_count_for_created_period() {
    let conn = crate::init_memory().unwrap();
    v59_seed_misparented_index(&conn);
    conn.execute("UPDATE memory_index_nodes SET child_count = 2 WHERE id = 'root-a1'", [])
        .unwrap();
    initialize(&conn).unwrap();
    // 2025-10 is created under the root: 2 -> 3 children.
    v59_assert_child_count_matches(&conn, "root-a1");
}

#[test]
fn v59_recomputes_root_child_count_for_removed_period() {
    let conn = crate::init_memory().unwrap();
    v59_node(&conn, "root-a1", None, "root", "Memory Root", None, None, "r0", None);
    v59_node(&conn, "period-a1-2025-07", Some("root-a1"), "period", "2025-07", None, None, "p1", None);
    v59_node(&conn, "period-a1-2025-08", Some("root-a1"), "period", "2025-08", None, None, "p2", None);
    v59_node(&conn, "session-a1-x", Some("period-a1-2025-07"), "session", "S", Some("x"), None, "s1", None);
    v59_node(&conn, "topic-x", Some("session-a1-x"), "topic", "x", Some("x"), Some("2025-08-03"), "t1", None);
    conn.execute("UPDATE memory_index_nodes SET child_count = 2 WHERE id = 'root-a1'", [])
        .unwrap();
    conn.execute_batch("PRAGMA user_version = 58").unwrap();
    initialize(&conn).unwrap();
    // 2025-07 is emptied and removed: 2 -> 1 child.
    let gone: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_index_nodes WHERE id = 'period-a1-2025-07'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(gone, 0);
    v59_assert_child_count_matches(&conn, "root-a1");
}
